// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::fs;
use std::fs::metadata;
use std::io;
use std::io::{BufRead, BufReader, Cursor, ErrorKind, Read, Seek, SeekFrom};
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, SystemTime};

use cedar_camera::abstract_camera::{AbstractCamera, Gain, Offset,
                                    bin_2x2, sample_2x2};
use cedar_camera::select_camera::{CameraInterface, select_camera};
use cedar_camera::image_camera::ImageCamera;

use canonical_error::{CanonicalError, CanonicalErrorCode};
use chrono::offset::Local;
use glob::glob;
use image::{GrayImage};
use image::ImageReader;
use image::codecs::jpeg::JpegEncoder;

use cedar_elements::cedar_common::CelestialCoord;
use cedar_elements::cedar_sky::{
    CatalogDescriptionResponse, CatalogEntry,
    CatalogEntryKey, CatalogEntryMatch,
    ConstellationResponse, ObjectTypeResponse, Ordering,
    QueryCatalogRequest, QueryCatalogResponse};
use cedar_elements::cedar_sky_trait::{CedarSkyTrait, LocationInfo};
use cedar_elements::solver_trait::SolverTrait;
use cedar_elements::wifi_trait::WifiTrait;

use nix::time::{ClockId, clock_gettime, clock_settime};
use nix::sys::time::TimeSpec;

use pico_args::Arguments;
use axum::Router;
use log::{debug, error, info, warn};
use prost::Message;
use tower_http::{services::ServeDir, cors::CorsLayer, cors::Any};
use tonic_web::GrpcWebLayer;

use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, registry, EnvFilter};
use tracing_appender::{non_blocking::NonBlockingBuilder};
use tracing_appender::rolling::{RollingFileAppender, Rotation};

use futures::join;

use crate::activity_led::ActivityLed;
use cedar_elements::astro_util::{
    alt_az_from_equatorial, equatorial_from_alt_az,
    fill_in_detections, magnitude_intensity_ratio, position_angle};
use cedar_elements::cedar::cedar_server::{Cedar, CedarServer};

use cedar_elements::cedar::{
    ActionRequest, CalibrationData, CameraModel,
    CelestialCoordChoice, CelestialCoordFormat,
    DisplayOrientation, EmptyMessage, FeatureLevel,
    FixedSettings, FovCatalogEntry, FrameRequest, FrameResult,
    Image, ImageCoord, LatLong, LocationBasedInfo, MountType,
    OperatingMode, OperationSettings, PlateSolution as PlateSolutionProto,
    ProcessingStats, Rectangle, StarCentroid, Preferences,
    ServerLogRequest, ServerLogResult, ServerInformation,
    WiFiAccessPoint};

use crate::calibrator::Calibrator;
use crate::detect_engine::{DetectEngine, DetectResult};
use crate::solve_engine::{PlateSolution, SolveEngine};
use crate::position_reporter::{TelescopePosition, create_alpaca_server};
use crate::motion_estimator::MotionEstimator;
use crate::polar_analyzer::PolarAnalyzer;
use cedar_elements::image_utils::{ImageRotator, scale_image};
use cedar_elements::value_stats::ValueStatsAccumulator;
use tetra3_server::tetra3_solver::Tetra3Solver;

use self::multiplex_service::MultiplexService;

fn tonic_status(canonical_error: CanonicalError) -> tonic::Status {
    tonic::Status::new(
        match canonical_error.code {
            CanonicalErrorCode::Unknown => tonic::Code::Unknown,
            CanonicalErrorCode::InvalidArgument => tonic::Code::InvalidArgument,
            CanonicalErrorCode::DeadlineExceeded => tonic::Code::DeadlineExceeded,
            CanonicalErrorCode::NotFound => tonic::Code::NotFound,
            CanonicalErrorCode::AlreadyExists => tonic::Code::AlreadyExists,
            CanonicalErrorCode::PermissionDenied => tonic::Code::PermissionDenied,
            CanonicalErrorCode::Unauthenticated => tonic::Code::Unauthenticated,
            CanonicalErrorCode::ResourceExhausted => tonic::Code::ResourceExhausted,
            CanonicalErrorCode::FailedPrecondition => tonic::Code::FailedPrecondition,
            CanonicalErrorCode::Aborted => tonic::Code::Aborted,
            CanonicalErrorCode::OutOfRange => tonic::Code::OutOfRange,
            CanonicalErrorCode::Unimplemented => tonic::Code::Unimplemented,
            CanonicalErrorCode::Internal => tonic::Code::Internal,
            CanonicalErrorCode::Unavailable => tonic::Code::Unavailable,
            CanonicalErrorCode::DataLoss => tonic::Code::DataLoss,
            // canonical_error module does not model Ok or Cancelled.
        },
        canonical_error.message)
}

struct MyCedar {
    // We organize our state as a sub-object so update_operation_settings() can
    // spawn a sub-task for the SETUP -> OPERATE mode transition; the sub-task
    // needs access to our state.
    state: Arc<tokio::sync::Mutex<CedarState>>,

    // Fake camera for using static image instead of an attached camera.
    test_image_camera: Option<Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>>,

    // Demo images, if any, that were found in ./demo_images directory.
    demo_images: Vec<String>,

    preferences_file: PathBuf,

    // The full path to our log file.
    log_file: PathBuf,

    product_name: String,
    copyright: String,
    feature_level: FeatureLevel,

    cedar_version: String,
    processor_model: String,
    os_version: String,
    serial_number: String,
}

struct CedarState {
    // The plate solver we are using.
    solver: Arc<tokio::sync::Mutex<dyn SolverTrait + Send + Sync>>,

    // The hardware camera that was detected, if any.
    attached_camera: Option<Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>>,

    // An exposure duration which is a good starting point for `attached_camera`.
    initial_exposure_duration: Duration,

    // The `camera` field is always populated with a usable AbstractCamera. This
    // will be one of:
    // * attached_camera
    // * a test image configured on command line
    // * a demo mode image
    // * a uniform gray image if none of the above are available.
    camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,

    fixed_settings: Arc<Mutex<FixedSettings>>,
    calibration_data: Arc<tokio::sync::Mutex<CalibrationData>>,
    operation_settings: OperationSettings,
    detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
    solve_engine: Arc<tokio::sync::Mutex<SolveEngine>>,
    calibrator: Arc<tokio::sync::Mutex<Calibrator>>,
    telescope_position: Arc<Mutex<TelescopePosition>>,
    polar_analyzer: Arc<Mutex<PolarAnalyzer>>,
    activity_led: Arc<Mutex<ActivityLed>>,

    // Not all builds of Cedar-server support Cedar-sky.
    cedar_sky: Option<Arc<Mutex<dyn CedarSkyTrait + Send>>>,

    // Not all builds of Cedar-server support Wifi control.
    wifi: Option<Arc<Mutex<dyn WifiTrait + Send>>>,

    // We host the user interface preferences and some operation settings here.
    // On startup we apply some of these to `operation_settings`; we reflect
    // them out to all clients and persist them to a server-side file.
    preferences: Arc<Mutex<Preferences>>,

    // This is the most recent display image returned by get_frame().
    scaled_image: Option<Arc<GrayImage>>,
    scaled_image_binning_factor: u32,
    scaled_image_rotation_size_ratio: f64,  // >= 1.0.
    scaled_image_frame_id: i32,

    // Image rotator used outside of focus mode or daylight align. We retain it
    // to cover momentary plate solve dropouts.
    image_rotator: Option<ImageRotator>,

    calibrating: bool,
    cancel_calibration: Arc<Mutex<bool>>,
    // Relevant only if calibration is underway (`calibration_image` is present).
    calibration_start: Instant,
    calibration_duration_estimate: Duration,

    // For focus assist.
    center_peak_position: Arc<Mutex<Option<ImageCoord>>>,

    serve_latency_stats: ValueStatsAccumulator,
    overall_latency_stats: ValueStatsAccumulator,

    // Some command line args.
    args_binning: Option<u32>,
    args_display_sampling: Option<bool>,
}

#[tonic::async_trait]
impl Cedar for MyCedar {
    async fn get_server_log(
        &self, request: tonic::Request<ServerLogRequest>)
        -> Result<tonic::Response<ServerLogResult>, tonic::Status>
    {
        let req: ServerLogRequest = request.into_inner();
        let tail = Self::read_log_tail(&self.log_file, req.log_request);
        if let Err(e) = tail {
            return Err(tonic::Status::failed_precondition(
                format!("Error reading log file {:?}: {:?}.", self.log_file, e)));
        }
        let mut response = ServerLogResult::default();
        response.log_content = tail.unwrap();

        Ok(tonic::Response::new(response))
    }

    async fn update_fixed_settings(
        &self, request: tonic::Request<FixedSettings>)
        -> Result<tonic::Response<FixedSettings>, tonic::Status>
    {
        let req: FixedSettings = request.into_inner();
        if let Some(observer_location) = req.observer_location {
            self.state.lock().await.fixed_settings.lock().unwrap().observer_location =
                Some(observer_location.clone());
            let preferences = Preferences{observer_location: Some(observer_location.clone()),
                                          ..Default::default()};
            self.update_preferences(tonic::Request::new(preferences)).await?;
            info!("Updated observer location to {:?}", observer_location);
        }
        if let Some(current_time) = req.current_time {
            let current_time =
                TimeSpec::new(current_time.seconds, current_time.nanos as i64);
            if let Err(e) = clock_settime(ClockId::CLOCK_REALTIME, current_time) {
                if let Ok(cur_time) = clock_gettime(ClockId::CLOCK_REALTIME) {
                    // If our current time is close to the client's time, just
                    // warn.
                    if (cur_time.tv_sec() - current_time.tv_sec()).abs() < 60 {
                        warn!("Could not update server time: {:?}", e);
                    } else {
                        error!("Could not update server time: {:?}", e);
                    }
                }
                // Either way, return an error to the client.
                // Note: the cedar-server binary needs CAP_SYS_TIME capability:
                // sudo setcap cap_sys_time+ep <path to cedar-server>
                return Err(tonic::Status::permission_denied(
                    format!("Error updating server time: {:?}", e)));
            }
            // Now that we know the correct date/time, initialize the solar system
            // object database.
            if let Some(cedar_sky) = &self.state.lock().await.cedar_sky {
                cedar_sky.lock().unwrap().initialize_solar_system(
                    SystemTime::now());
            }
            info!("Updated server time to {:?}", Local::now());
            // Don't store the client time in our fixed_settings state, but
            // arrange to return our current time.
        }
        if let Some(_session_name) = req.session_name {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateFixedSettings not implemented for session_name."));
        }
        if let Some(_max_exposure_time) = req.max_exposure_time {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateFixedSettings cannot update max_exposure_time."));
        }
        let mut fixed_settings =
            self.state.lock().await.fixed_settings.lock().unwrap().clone();
        // Fill in our current time.
        Self::fill_in_time(&mut fixed_settings);
        Ok(tonic::Response::new(fixed_settings))
    }

    async fn update_operation_settings(
        &self, request: tonic::Request<OperationSettings>)
        -> Result<tonic::Response<OperationSettings>, tonic::Status> {
        let mut req: OperationSettings = request.into_inner();
        if let Some(new_operating_mode) = req.operating_mode {
            let mut locked_state = self.state.lock().await;
            // Only do something if operating mode is changing.
            if new_operating_mode != locked_state.operation_settings.operating_mode.unwrap() {
                if locked_state.calibrating {
                    return Err(tonic::Status::failed_precondition(
                        "Cannot change operating mode while calibrating"));
                }
                let focus_mode = locked_state.operation_settings.focus_assist_mode.unwrap();
                let daylight_mode = locked_state.operation_settings.daylight_mode.unwrap();
                let mut final_focus_mode = focus_mode;
                if let Some(req_focus_assist_mode) = req.focus_assist_mode {
                    final_focus_mode = req_focus_assist_mode;
                    // We're handling this here, so don't also handle it below.
                    req.focus_assist_mode = None;
                }
                let mut final_daylight_mode = daylight_mode;
                if let Some(req_daylight_mode) = req.daylight_mode {
                    final_daylight_mode = req_daylight_mode;
                    // We're handling this here, so don't also handle it below.
                    req.daylight_mode = None;
                }

                let mut calibrating = false;
                if new_operating_mode == OperatingMode::Setup as i32 {
                    // Transition: OPERATE -> SETUP mode. We are already calibrated.
                    Self::set_gain(&mut locked_state, final_daylight_mode).await;
                    if final_focus_mode {
                        // In SETUP focus assist mode we run at full speed with
                        // pre-calibrate settings.
                        if let Err(x) = Self::set_update_interval(
                            &*locked_state, Duration::ZERO).await
                        {
                            return Err(tonic_status(x));
                        }
                        if let Err(x) = Self::set_pre_calibration_defaults(
                            &mut locked_state).await
                        {
                            return Err(tonic_status(x));
                        }
                    }
                    locked_state.solve_engine.lock().await.set_align_mode(true).await;
                    locked_state.image_rotator = None;
                    Self::reset_session_stats(locked_state.deref_mut()).await;
                    {
                        let mut locked_detect_engine =
                            locked_state.detect_engine.lock().await;
                        locked_detect_engine.set_focus_mode(final_focus_mode);
                        locked_detect_engine.set_daylight_mode(final_daylight_mode);
                        if final_focus_mode {
                            locked_detect_engine.set_calibrated_exposure_duration(None);
                        }
                    }
                    locked_state.telescope_position.lock().unwrap().slew_active = false;
                } else if new_operating_mode == OperatingMode::Operate as i32 {
                    // Transition: SETUP -> OPERATE mode.
                    if focus_mode || daylight_mode {
                        // The SETUP (with focus mode or daytime align) ->
                        // OPERATE mode change involves a call to calibrate(),
                        // which can take several seconds. If the gRPC client
                        // aborts the RPC (e.g. due to timeout), we want the
                        // calibration and state updates (i.e. detect engine's
                        // focus_mode, our operating_mode) to be completed
                        // properly.
                        Self::spawn_calibration(self.state.clone(),
                                                /*new_operate_mode=*/true,
                                                /*new_focus_mode=*/false,
                                                /*new_daylight_mode=*/false);
                        calibrating = true;
                        // The update of state.operation_settings.operation_mode
                        // happens when the calibration finishes. TODO: also focus
                        // and daylight sub-modes.
                    } else {
                        // Transition into Operate mode from SETUP align mode. Already
                        // calibrated.
                        Self::set_gain(&mut locked_state, /*daylight_mode=*/false).await;
                        locked_state.detect_engine.lock().await.set_daylight_mode(false);
                        locked_state.solve_engine.lock().await.set_align_mode(false).await;
                        locked_state.solve_engine.lock().await.start().await;
                        // Restore OPERATE mode update interval.
                        let update_interval = locked_state.operation_settings.
                            update_interval.clone().unwrap();
                        let std_duration = std::time::Duration::try_from(
                            update_interval).unwrap();
                        if let Err(x) = Self::set_update_interval(
                            &*locked_state, std_duration).await
                        {
                            return Err(tonic_status(x));
                        }
                    }
                } else {
                    return Err(tonic::Status::invalid_argument(
                        format!("Got invalid operating_mode: {}.", new_operating_mode)));
                }
                if !calibrating {
                    locked_state.operation_settings.operating_mode =
                        Some(new_operating_mode);
                    locked_state.operation_settings.daylight_mode =
                        Some(final_daylight_mode);
                    locked_state.operation_settings.focus_assist_mode =
                        Some(final_focus_mode);
                }
            }  // Operating mode is changing.
        }  // Update operating_mode.
        if let Some(new_daylight_mode) = req.daylight_mode {
            let mut locked_state = self.state.lock().await;
            let mut calibrating = false;
            if locked_state.operation_settings.daylight_mode.unwrap()
                != new_daylight_mode
            {
                if locked_state.calibrating {
                    return Err(tonic::Status::failed_precondition(
                        "Cannot change daylight mode while calibrating"));
                }
                let mut final_focus_mode =
                    locked_state.operation_settings.focus_assist_mode.unwrap();
                if let Some(req_focus_assist_mode) = req.focus_assist_mode {
                    final_focus_mode = req_focus_assist_mode;
                    // We're handling this here, so don't also handle it below.
                    req.focus_assist_mode = None;
                }
                if locked_state.operation_settings.operating_mode ==
                    Some(OperatingMode::Setup as i32)
                {
                    // In SETUP align mode?
                    if !locked_state.operation_settings.focus_assist_mode.unwrap()
                        && !new_daylight_mode
                    {
                        // Turning off daylight_mode in SETUP align mode;
                        // need calibration, which can take several seconds.
                        // If the gRPC client aborts the RPC (e.g. due to
                        // timeout), we want the calibration and state
                        // updates (i.e. detect engine's focus_mode, our
                        // operating_mode) to be completed properly.
                        Self::spawn_calibration(self.state.clone(),
                                                /*new_operate_mode=*/false,
                                                final_focus_mode,
                                                new_daylight_mode);
                        calibrating = true;
                    } else {
                        Self::set_gain(&mut locked_state, new_daylight_mode).await;
                    }
                }
            }
            if !calibrating {
                locked_state.detect_engine.lock().await.set_daylight_mode(
                    new_daylight_mode);
                locked_state.operation_settings.daylight_mode = Some(new_daylight_mode);
            }
        }
        if let Some(new_focus_assist_mode) = req.focus_assist_mode {
            let mut locked_state = self.state.lock().await;
            let mut calibrating = false;
            if locked_state.operation_settings.operating_mode ==
                Some(OperatingMode::Setup as i32) &&
                locked_state.operation_settings.focus_assist_mode.unwrap()
                != new_focus_assist_mode
            {
                if locked_state.calibrating {
                    return Err(tonic::Status::failed_precondition(
                        "Cannot change focus assist mode while calibrating"));
                }
                let daylight_mode = locked_state.operation_settings.daylight_mode.unwrap();
                if new_focus_assist_mode {
                    // Entering focus assist mode.
                    // Run at full speed for focus assist.
                    if let Err(x) = Self::set_update_interval(
                        &*locked_state, Duration::ZERO).await
                    {
                        return Err(tonic_status(x));
                    }
                    if let Err(x) = Self::set_pre_calibration_defaults(
                        &mut locked_state).await
                    {
                        return Err(tonic_status(x));
                    }
                    Self::set_gain(&mut locked_state, daylight_mode).await;
                } else if !daylight_mode {
                    // Exiting focus assist mode, without daylight mode active.
                    // Trigger a calibration, which can take several seconds. If
                    // the gRPC client aborts the RPC (e.g. due to timeout), we
                    // want the calibration and state updates (i.e. detect
                    // engine's focus_mode, our operating_mode) to be completed
                    // properly.
                    Self::spawn_calibration(self.state.clone(),
                                            /*new_operate_mode=*/false,
                                            new_focus_assist_mode,
                                            daylight_mode);
                    calibrating = true;
                }
            }
            if !calibrating {
                locked_state.detect_engine.lock().await.set_focus_mode(new_focus_assist_mode);
                locked_state.operation_settings.focus_assist_mode =
                    Some(new_focus_assist_mode);
            }
        }
        if let Some(update_interval) = req.update_interval {
            if update_interval.seconds < 0 || update_interval.nanos < 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got negative update_interval: {}.", update_interval)));
            }
            let std_duration = std::time::Duration::try_from(
                update_interval.clone()).unwrap();
            {
                let mut locked_state = self.state.lock().await;
                if locked_state.operation_settings.operating_mode ==
                    Some(OperatingMode::Operate as i32) ||
                    !locked_state.operation_settings.focus_assist_mode.unwrap()
                {
                    if let Err(x) = Self::set_update_interval(&*locked_state,
                                                              std_duration).await {
                        return Err(tonic_status(x));
                    }
                }
                locked_state.operation_settings.update_interval =
                    Some(update_interval.clone());
            }
            let preferences = Preferences{update_interval: Some(update_interval),
                                          ..Default::default()};
            self.update_preferences(tonic::Request::new(preferences)).await?;
        }
        if let Some(_dwell_update_interval) = req.dwell_update_interval {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for dwell_update_interval."));
        }
        if let Some(_log_dwelled_positions) = req.log_dwelled_positions {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for log_dwelled_positions."));
        }
        if let Some(catalog_entry_match) = req.catalog_entry_match {
            {
                let mut locked_state = self.state.lock().await;
                if locked_state.cedar_sky.is_none() {
                    return Err(tonic::Status::unimplemented(
                        format!("{} does not include Cedar Sky.", self.product_name)));
                }
                locked_state.operation_settings.catalog_entry_match =
                    Some(catalog_entry_match.clone());
                locked_state.solve_engine.lock().await.set_catalog_entry_match(
                    Some(catalog_entry_match.clone())).await;
            }
            let preferences = Preferences{
                catalog_entry_match: Some(catalog_entry_match),
                ..Default::default()};
            self.update_preferences(tonic::Request::new(preferences)).await?;
        }
        if let Some(demo_image_filename) = req.demo_image_filename {
            let mut locked_state = self.state.lock().await;
            if demo_image_filename.is_empty() {
                // Go back to using our configured camera.
                locked_state.camera = get_camera(&locked_state.attached_camera,
                                                 &self.test_image_camera);
                locked_state.operation_settings.demo_image_filename = None;
            } else {
                let input_path = PathBuf::from("./demo_images").join(
                    demo_image_filename.clone());
                let img_file = match ImageReader::open(&input_path) {
                    Err(x) => {
                        return Err(tonic::Status::failed_precondition(
                            format!("Error opening image file {:?}: {:?}.",
                                    input_path, x)));
                    },
                    Ok(img_file) => img_file
                };
                let img = match img_file.decode() {
                    Err(x) => {
                        return Err(tonic::Status::failed_precondition(
                            format!("Error decoding image file {:?}: {:?}.",
                                    input_path, x)));
                    },
                    Ok(img) => img
                };
                let img_u8 = img.to_luma8();
                locked_state.camera =
                    Arc::new(tokio::sync::Mutex::new(
                        Box::new(ImageCamera::new(img_u8).unwrap())));
                locked_state.operation_settings.demo_image_filename =
                    Some(demo_image_filename);
            }
            let new_camera = locked_state.camera.clone();
            let (width, height) = new_camera.lock().await.dimensions();

            let update_interval = locked_state.operation_settings.
                update_interval.clone().unwrap();
            let mut std_duration = std::time::Duration::try_from(
                update_interval).unwrap();
            if locked_state.operation_settings.operating_mode ==
                Some(OperatingMode::Setup as i32) &&
                locked_state.operation_settings.focus_assist_mode.unwrap()
            {
                std_duration = Duration::ZERO;  // Fast update mode for focusing.
            }
            if let Err(x) = Self::set_update_interval(&*locked_state,
                                                      std_duration).await {
                return Err(tonic_status(x));
            }
            let (binning, display_sampling) =
                Self::compute_binning(&locked_state, width as u32, height as u32);
            locked_state.detect_engine.lock().await.set_binning(binning, display_sampling);
            locked_state.detect_engine.lock().await.replace_camera(new_camera.clone());
            locked_state.calibrator.lock().await.replace_camera(new_camera.clone());

            // Validate boresight_pixel, to make sure it is still within the
            // image area.
            let bsp =
            {
                locked_state.preferences.lock().unwrap().boresight_pixel.clone()
            };
            if let Some(bsp) = bsp {
                let inset = 16;
                if bsp.x < inset as f64 || bsp.x > (width - inset) as f64 ||
                    bsp.y < inset as f64 || bsp.y > (height - inset) as f64
                {
                    locked_state.preferences.lock().unwrap().boresight_pixel = None;
                    locked_state.solve_engine.lock().await.set_boresight_pixel(None).
                        await.unwrap();
                }
            };
        }
        if let Some(invert_camera) = req.invert_camera {
            {
                let mut locked_state = self.state.lock().await;
                locked_state.operation_settings.invert_camera =
                    Some(invert_camera);
                if let Some(attached_camera) = &locked_state.attached_camera {
                    attached_camera.lock().await.set_inverted(invert_camera).unwrap();
                }
            }
            let preferences = Preferences{
                invert_camera: Some(invert_camera),
                ..Default::default()};
            self.update_preferences(tonic::Request::new(preferences)).await?;
        }

        Ok(tonic::Response::new(self.state.lock().await.operation_settings.clone()))
    }  // update_operation_settings().

    async fn update_preferences(
        &self, request: tonic::Request<Preferences>)
        -> Result<tonic::Response<Preferences>, tonic::Status> {
        let req: Preferences = request.into_inner();
        // Hold our lock across this entire operation to ensure that the file
        // update is done one at a time.
        let locked_state = self.state.lock().await;

        let mut our_prefs = locked_state.preferences.lock().unwrap().clone();
        if let Some(coord_format) = req.celestial_coord_format {
            our_prefs.celestial_coord_format = Some(coord_format);
        }
        if let Some(eyepiece_fov) = req.eyepiece_fov {
            our_prefs.eyepiece_fov = Some(eyepiece_fov);
        }
        if let Some(night_vision) = req.night_vision_theme {
            our_prefs.night_vision_theme = Some(night_vision);
        }
        if let Some(hide_app_bar) = req.hide_app_bar {
            our_prefs.hide_app_bar = Some(hide_app_bar);
        }
        if let Some(mount_type) = req.mount_type {
            if self.feature_level == FeatureLevel::Basic {
                return Err(tonic::Status::invalid_argument(
                    "Cannot set mount type at Basic feature level"));
            }
            our_prefs.mount_type = Some(mount_type);
        }
        if let Some(observer_location) = req.observer_location {
            our_prefs.observer_location = Some(observer_location);
        }
        if let Some(update_interval) = req.update_interval {
            our_prefs.update_interval = Some(update_interval);
        }
        if let Some(catalog_entry_match) = req.catalog_entry_match {
            our_prefs.catalog_entry_match = Some(catalog_entry_match);
        }
        if let Some(max_distance_active) = req.max_distance_active {
            our_prefs.max_distance_active = Some(max_distance_active);
        }
        if let Some(max_distance) = req.max_distance {
            our_prefs.max_distance = Some(max_distance);
        }
        if let Some(min_elevation_active) = req.min_elevation_active {
            our_prefs.min_elevation_active = Some(min_elevation_active);
        }
        if let Some(min_elevation) = req.min_elevation {
            our_prefs.min_elevation = Some(min_elevation);
        }
        if let Some(ordering) = req.ordering {
            our_prefs.ordering = Some(ordering);
        }
        if let Some(advanced) = req.advanced {
            our_prefs.advanced = Some(advanced);
        }
        if let Some(text_size_index) = req.text_size_index {
            our_prefs.text_size_index = Some(text_size_index);
        }
        if let Some(boresight_pixel) = req.boresight_pixel {
            our_prefs.boresight_pixel = Some(boresight_pixel);
        }
        if let Some(invert_camera) = req.invert_camera {
            our_prefs.invert_camera = Some(invert_camera);
        }
        if let Some(right_handed) = req.right_handed {
            our_prefs.right_handed = Some(right_handed);
        }
        if let Some(celestial_coord_choice) = req.celestial_coord_choice {
            our_prefs.celestial_coord_choice = Some(celestial_coord_choice);
        }
        if let Some(screen_always_on) = req.screen_always_on {
            our_prefs.screen_always_on = Some(screen_always_on);
        }
        *locked_state.preferences.lock().unwrap() = our_prefs.clone();

        // Write updated preferences to file. Note that this operation is
        // guarded by our holding locked_state.
        Self::write_preferences_file(&self.preferences_file, &our_prefs);

        Ok(tonic::Response::new(our_prefs.clone()))
    }

    async fn get_frame(&self, request: tonic::Request<FrameRequest>)
                       -> Result<tonic::Response<FrameResult>, tonic::Status> {
        self.state.lock().await.activity_led.lock().unwrap().received_rpc();
        let req: FrameRequest = request.into_inner();
        let non_blocking = req.non_blocking.is_some() && req.non_blocking.unwrap();
        let landscape = req.display_orientation.is_none() ||
            req.display_orientation.unwrap() == DisplayOrientation::Landscape as i32;
        let fr = Self::get_next_frame(
            self.state.clone(), req.prev_frame_id, non_blocking, landscape).await;
        let mut frame_result = FrameResult {..Default::default()};
        if fr.is_none() {
	    assert!(non_blocking);
            frame_result.has_result = Some(false);
        } else {
            frame_result = fr.unwrap();
            if non_blocking {
                frame_result.has_result = Some(true);
            }
        }
        frame_result.server_information = Some(self.get_server_information().await);
        Ok(tonic::Response::new(frame_result))
    }

    async fn initiate_action(&self, request: tonic::Request<ActionRequest>)
                             -> Result<tonic::Response<EmptyMessage>, tonic::Status> {
        let req: ActionRequest = request.into_inner();
        if req.cancel_calibration.unwrap_or(false) {
            let locked_state = self.state.lock().await;
            if locked_state.calibrating {
                *locked_state.cancel_calibration.lock().unwrap() = true;
                locked_state.solver.lock().await.cancel();
            }
        }
        if req.capture_boresight.unwrap_or(false) {
            let operating_mode =
                self.state.lock().await.operation_settings.operating_mode.or(
                    Some(OperatingMode::Setup as i32)).unwrap();
            if operating_mode == OperatingMode::Setup as i32 {
                return Err(tonic::Status::failed_precondition(
                    "Capture boresight not valid in setup mode."));
            }
            // Operate mode.
            let locked_state = self.state.lock().await;
            let plate_solution = locked_state.solve_engine.lock().await.
                get_next_result(None, /*non_blocking=*/false).await.unwrap();
            if let Some(slew_request) = plate_solution.slew_request {
                let bsp = slew_request.image_pos.unwrap();
                if let Err(x) = locked_state.solve_engine.lock().await.
                    set_boresight_pixel(Some(bsp.clone())).await
                {
                    return Err(tonic_status(x));
                }
                let preferences = Preferences{
                    boresight_pixel: Some(bsp.clone()),
                    ..Default::default()};
                self.update_preferences(tonic::Request::new(preferences)).await?;
            } else {
                return Err(tonic::Status::failed_precondition(
                    format!("No slew request active")));
            }
        }  // capture_boresight.
        if let Some(mut bsp) = req.designate_boresight {
            let focus_mode;
            let daylight_mode;
            let image_rotator;
            let width;
            let height;
            {
                let locked_state = self.state.lock().await;
                focus_mode = locked_state.operation_settings.focus_assist_mode.unwrap();
                daylight_mode = locked_state.operation_settings.daylight_mode.unwrap();
                image_rotator = locked_state.image_rotator.clone();
                (width, height) = locked_state.camera.lock().await.dimensions();
            }
            if !focus_mode && !daylight_mode && image_rotator.is_some() {
                (bsp.x, bsp.y) = image_rotator.unwrap().transform_from_rotated(
                    bsp.x, bsp.y, width as u32, height as u32);
            }

            if let Err(x) = self.state.lock().await.solve_engine.lock().await.
                set_boresight_pixel(Some(bsp.clone())).await
            {
                return Err(tonic_status(x));
            };
            let preferences = Preferences{
                boresight_pixel: Some(bsp.clone()),
                ..Default::default()};
            self.update_preferences(tonic::Request::new(preferences)).await?;
        }
        if req.shutdown_server.unwrap_or(false) {
            info!("Shutting down host system");
            self.state.lock().await.activity_led.lock().unwrap().stop();
            let output = Command::new("sudo")
                .arg("shutdown")
                .arg("now")
                .output()
                .expect("Failed to execute 'sudo shutdown now' command");
            if !output.status.success() {
                let error_str = String::from_utf8_lossy(&output.stderr);
                    return Err(tonic::Status::failed_precondition(
                        format!("sudo shutdown error: {:?}.", error_str)));
            }
        }
        if req.restart_server.unwrap_or(false) {
            info!("Restarting host system");
            self.state.lock().await.activity_led.lock().unwrap().stop();
            let output = Command::new("sudo")
                .arg("reboot")
                .arg("now")
                .output()
                .expect("Failed to execute 'sudo reboot now' command");
            if !output.status.success() {
                let error_str = String::from_utf8_lossy(&output.stderr);
                    return Err(tonic::Status::failed_precondition(
                        format!("sudo reboot error: {:?}.", error_str)));
            }
        }
        if let Some(slew_coord) = req.initiate_slew {
            let locked_state = self.state.lock().await;
            let mount_type = locked_state.preferences.lock().unwrap().mount_type;
            if mount_type == Some(MountType::AltAz.into()) &&
                locked_state.fixed_settings.lock().unwrap().observer_location.is_none()
            {
                return Err(tonic::Status::failed_precondition(
                    "Need observer location for goto with alt-az mount"));
            }
            let mut telescope = locked_state.telescope_position.lock().unwrap();
            telescope.slew_target_ra = slew_coord.ra;
            telescope.slew_target_dec = slew_coord.dec;
            telescope.slew_active = true;
        }
        if req.stop_slew.unwrap_or(false) {
            let locked_state = self.state.lock().await;
            locked_state.telescope_position.lock().unwrap().slew_active = false;
        }
        if req.save_image.unwrap_or(false) {
            let locked_state = self.state.lock().await;
            let solve_engine = &mut locked_state.solve_engine.lock().await;
            // TODO: don't hold our state.lock() for this.
            if let Err(x) = solve_engine.save_image().await {
                return Err(tonic_status(x));
            }
        }
        if let Some(update_ap) = req.update_wifi_access_point {
	    let wifi = self.state.lock().await.wifi.clone();
	    if wifi.is_none() {
                return Err(tonic::Status::unimplemented(
                    format!("{} does not include WiFi control.", self.product_name)));
	    }
            let mut locked_wifi = wifi.as_ref().unwrap().lock().unwrap();
            if let Err(x) = locked_wifi.update_access_point(
                update_ap.channel,
                update_ap.ssid.as_deref(),
                update_ap.psk.as_deref())
            {
                return Err(tonic_status(x));
            }
        }
        Ok(tonic::Response::new(EmptyMessage{}))
    }  // initiate_action().

    async fn query_catalog_entries(
        &self, request: tonic::Request<QueryCatalogRequest>)
        -> Result<tonic::Response<QueryCatalogResponse>, tonic::Status>
    {
        let locked_state = self.state.lock().await;
        if locked_state.cedar_sky.is_none() {
            return Err(tonic::Status::unimplemented("Cedar Sky is not present"));
        }
        let req: QueryCatalogRequest = request.into_inner();
        let limit_result = match req.limit_result {
            Some(l) => Some(l as usize),
            None => None,
        };
        let ordering = match req.ordering {
            Some(1) => Some(Ordering::Brightness),
            Some(2) => Some(Ordering::SkyLocation),
            Some(3) => Some(Ordering::Elevation),
            _ => Some(Ordering::Brightness),
        };
        let catalog_entry_match = req.catalog_entry_match.as_ref().unwrap();

        let plate_solution = locked_state.solve_engine.lock().await.
            get_next_result(None, /*non_blocking=*/false).await.unwrap();
        let sky_location =
            if let Some(psp) = plate_solution.plate_solution.as_ref() {
                if psp.target_sky_coord.len() > 0 {
                    Some(psp.target_sky_coord[0].clone())
                } else {
                    psp.image_sky_coord.clone()
                }
            } else {
                None
            };
        let fixed_settings = locked_state.fixed_settings.lock();
        let location_info =
            if let Some(obs_loc) = &fixed_settings.unwrap().observer_location {
                Some(LocationInfo {
                    observer_location: obs_loc.clone(),
                    observing_time: SystemTime::now(),
                })
            } else {
                None
            };

        let result =
            locked_state.cedar_sky.as_ref().unwrap().lock().unwrap().query_catalog_entries(
                req.max_distance,
                req.min_elevation,
                catalog_entry_match.faintest_magnitude,
                catalog_entry_match.match_catalog_label,
                &catalog_entry_match.catalog_label,
                catalog_entry_match.match_object_type_label,
                &catalog_entry_match.object_type_label,
                req.text_search,
                ordering,
                req.decrowd_distance,
                limit_result,
                sky_location,
                location_info);
        if let Err(e) = result {
            return Err(tonic_status(e));
        }
        let (entries, truncated_count) = result.unwrap();

        let mut response = QueryCatalogResponse::default();
        for entry in entries {
            response.entries.push(entry);
        }
        response.truncated_count = truncated_count as i32;

        Ok(tonic::Response::new(response))
    }  // query_catalog_entries().

    async fn get_catalog_entry(
        &self, request: tonic::Request<CatalogEntryKey>)
        -> Result<tonic::Response<CatalogEntry>, tonic::Status>
    {
        let locked_state = self.state.lock().await;
        if locked_state.cedar_sky.is_none() {
            return Err(tonic::Status::unimplemented("Cedar Sky is not present"));
        }
        let req: CatalogEntryKey = request.into_inner();

        let x = locked_state.cedar_sky.as_ref().unwrap().lock().unwrap().get_catalog_entry(
            req, SystemTime::now());
        match x {
            Ok(entry) => {
                Ok(tonic::Response::new(entry))
            },
            Err(e) => {
                return Err(tonic_status(e));
            }
        }
    }  // get_catalog_entry().

    async fn get_catalog_descriptions(
        &self, _request: tonic::Request<EmptyMessage>)
        -> Result<tonic::Response<CatalogDescriptionResponse>, tonic::Status>
    {
        let locked_state = self.state.lock().await;
        if locked_state.cedar_sky.is_none() {
            return Err(tonic::Status::unimplemented("Cedar Sky is not present"));
        }
        let catalog_descriptions =
            locked_state.cedar_sky.as_ref().unwrap().lock().unwrap()
            .get_catalog_descriptions();
        let mut response = CatalogDescriptionResponse::default();
        for cd in catalog_descriptions {
            response.catalog_descriptions.push(cd);
        }

        Ok(tonic::Response::new(response))
    }

    async fn get_object_types(
        &self, _request: tonic::Request<EmptyMessage>)
        -> Result<tonic::Response<ObjectTypeResponse>, tonic::Status>
    {
        let locked_state = self.state.lock().await;
        if locked_state.cedar_sky.is_none() {
            return Err(tonic::Status::unimplemented("Cedar Sky is not present"));
        }
        let object_types =
            locked_state.cedar_sky.as_ref().unwrap().lock().unwrap().get_object_types();

        let mut response = ObjectTypeResponse::default();
        for ot in object_types {
            response.object_types.push(ot);
        }

        Ok(tonic::Response::new(response))
    }

    async fn get_constellations(
        &self, _request: tonic::Request<EmptyMessage>)
        -> Result<tonic::Response<ConstellationResponse>, tonic::Status>
    {
        let locked_state = self.state.lock().await;
        if locked_state.cedar_sky.is_none() {
            return Err(tonic::Status::unimplemented("Cedar Sky is not present"));
        }
        let constellations =
            locked_state.cedar_sky.as_ref().unwrap().lock().unwrap().get_constellations();

        let mut response = ConstellationResponse::default();
        for c in constellations {
            response.constellations.push(c);
        }

        Ok(tonic::Response::new(response))
    }
}  // impl Cedar for MyCedar.

impl MyCedar {
    fn get_demo_images() -> Result<Vec<String>, tonic::Status> {
        let dir = Path::new("./demo_images");
        if !dir.exists() {
            return Err(tonic::Status::failed_precondition(
                format!("The path {:?} is not found", dir)));
        }
        if !dir.is_dir() {
            return Err(tonic::Status::failed_precondition(
                format!("The path {:?} is not a directory", dir)));
        }
        let mut response = Vec::<String>::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let extension = path.extension().unwrap_or_default();
            if extension == "jpg" || extension == "bmp" {
                let file_name = path.file_name().unwrap().to_str().unwrap();
                response.push(file_name.to_string());
            }
        }
        Ok(response)
    }

    // See "About Resolutions" below.
    // Computes (binning, display_sampling) for camera, taking optional command line
    // overrides into account.
    // Returns:
    // binning: u32; whether (and how much, 2x2 or 4x4) the acquired image
    //     is binned prior to CedarDetect and sending to the UI.
    // display_sampling: bool; whether (possibly binned) image is to be 2x
    //     sampled when sending to the
    fn compute_binning(state: &CedarState, width: u32, height: u32) -> (u32, bool)
    {
        let args_binning = state.args_binning;
        let args_display_sampling = state.args_display_sampling;
        let mpix = (width * height) as f64 / 1000000.0;
        // Initialize binning/sampling parameters based on sensor resolution.
        let mut binning = 1_u32;
        let mut display_sampling = false;
        if mpix <= 0.75 {
            // Use initial values.
        } else if mpix <= 3.0 {
            binning = 2;
        } else if mpix <= 12.0 {
            binning = 4;
        } else {
            binning = 4;
            display_sampling = true;
        }
        // Allow command-line overrides of sampling/binning parameters.
        if let Some(ba) = args_binning {
            binning = ba;
        }
        if let Some(dsa) = args_display_sampling {
            display_sampling = dsa;
        }
        debug!("For {:.1}mpix, binning {}, display_sampling {}",
               mpix, binning, display_sampling);
        (binning, display_sampling)
    }

    // When we leave SETUP (with focus mode), either because focus mode is
    // turned off (transitioning to SETUP align mode), or because of
    // transitioning to OPERATE, we need to calibrate.
    // When the calibration finishes, the `new_operate_mode`, `new_focus_mode`,
    // and `new_daylight_mode` args are used to set the post-calibration
    // operating mode. If the calibration is aborted, the current operating mode
    // is retained.
    fn spawn_calibration(state: Arc<tokio::sync::Mutex<CedarState>>,
                         new_operate_mode: bool,
                         new_focus_mode: bool,
                         new_daylight_mode: bool) {
        // The calibrate() call can take several seconds. If the gRPC client
        // aborts the RPC (e.g. due to timeout), we want the calibration and
        // state updates (i.e. detect engine's focus_mode, our operating_mode)
        // to be completed properly.
        //
        // The spawned task runs to completion even if the RPC handler task
        // aborts.
        //
        // Note that below we return immediately rather than joining the
        // task_handle. We arrange for get_frame() to return a FrameResult with
        // a information about the ongoing calibration.

        let _task_handle: tokio::task::JoinHandle<Result<(), tonic::Status>> =
            tokio::task::spawn(async move {
                {
                    let mut locked_state = state.lock().await;
                    if locked_state.calibrating {
                        return Ok(());  // Already in flight.
                    }
                    let calibration_solve_timeout =
                        locked_state.solver.lock().await.default_timeout();

                    Self::set_gain(&mut locked_state, /*daylight_mode=*/false).await;
                    locked_state.calibrating = true;
                    locked_state.calibration_start = Instant::now();
                    locked_state.calibration_duration_estimate =
                        Duration::from_secs(5) + calibration_solve_timeout;
                    locked_state.calibration_data.lock().await
                        .calibration_time =
                        Some(prost_types::Timestamp::try_from(
                            SystemTime::now()).unwrap());
                }
                // No locks held.
                let succeeded = match Self::calibrate(state.clone()).await {
                    Ok(s) => s,
                    Err(e) => {
                        // The only error we expect is Aborted.
                        assert!(e.code == CanonicalErrorCode::Aborted);
                        false
                    },
                };

                let mut locked_state = state.lock().await;
                locked_state.calibrating = false;
                if *locked_state.cancel_calibration.lock().unwrap() || !succeeded {
                    // Calibration failed or was cancelled. Stay in current mode.
                    *locked_state.cancel_calibration.lock().unwrap() = false;
                    let focus_mode = locked_state.operation_settings.focus_assist_mode.unwrap();
                    let daylight_mode = locked_state.operation_settings.daylight_mode.unwrap();
                    Self::set_gain(&mut locked_state, daylight_mode).await;
                    let mut locked_detect_engine =
                        locked_state.detect_engine.lock().await;
                    locked_detect_engine.set_focus_mode(focus_mode);
                    locked_detect_engine.set_daylight_mode(daylight_mode);
                } else {
                    // Calibration completed.
                    Self::set_gain(&mut locked_state, new_daylight_mode).await;
                    locked_state.detect_engine.lock().await
                        .set_daylight_mode(new_daylight_mode);
                    locked_state.operation_settings.daylight_mode =
                        Some(new_daylight_mode);
                    locked_state.detect_engine.lock().await.set_focus_mode(new_focus_mode);
                    locked_state.operation_settings.focus_assist_mode =
                        Some(new_focus_mode);
                    if new_operate_mode {
                        // Transition into Operate mode.
                        locked_state.detect_engine.lock().await.set_focus_mode(false);
                        locked_state.detect_engine.lock().await.set_daylight_mode(false);
                        locked_state.solve_engine.lock().await.set_align_mode(false).await;
                        locked_state.solve_engine.lock().await.start().await;
                        // Restore OPERATE mode update interval.
                        let update_interval = locked_state.operation_settings.
                            update_interval.clone().unwrap();
                        let std_duration = std::time::Duration::try_from(
                            update_interval).unwrap();
                        if let Err(x) = Self::set_update_interval(
                            &*locked_state, std_duration).await
                        {
                            return Err(tonic_status(x));
                        }
                        locked_state.operation_settings.operating_mode =
                            Some(OperatingMode::Operate as i32);
                    }
                }
                Ok(())
            });
        // Let _task_handle go out of scope, detaching the spawned calibration
        // task to complete regardless of a possible RPC timeout.
    }

    fn write_preferences_file(preferences_file: &PathBuf, preferences: &Preferences) {
        // Write updated preferences to file.
        let prefs_path = Path::new(preferences_file);
        let scratch_path = prefs_path.with_extension("tmp");

        let mut buf = vec![];
        if let Err(e) = preferences.encode(&mut buf) {
            warn!("Could not encode preferences: {:?}", e);
            return;
        }
        if let Err(e) = fs::write(&scratch_path, buf) {
            warn!("Could not write file {:?}: {:?}", &scratch_path, e);
            return;
        }
        if let Err(e) = fs::rename(&scratch_path, &prefs_path) {
            warn!("Could not rename file {:?} to {:?}: {:?}",
                  &scratch_path, &prefs_path, e);
            return;
        }
    }

    async fn get_server_information(&self) -> ServerInformation {
        let camera =
            if let Some(test_image_camera) = &self.test_image_camera {
                let locked_camera = test_image_camera.lock().await;
                Some(CameraModel{
                    model: locked_camera.model(),
                    image_width: locked_camera.dimensions().0,
                    image_height: locked_camera.dimensions().1,
                })
            } else if let Some(attached_camera) = &self.state.lock().await.attached_camera {
                let locked_camera = attached_camera.lock().await;
                Some(CameraModel{
                    model: locked_camera.model(),
                    image_width: locked_camera.dimensions().0,
                    image_height: locked_camera.dimensions().1,
                })
            } else {
                None
            };
        let mut server_info = ServerInformation {
            product_name: self.product_name.clone(),
            copyright: self.copyright.clone(),
            cedar_server_version: self.cedar_version.clone(),
            feature_level: self.feature_level as i32,
            processor_model: self.processor_model.clone(),
            os_version: self.os_version.clone(),
            serial_number: self.serial_number.clone(),
            cpu_temperature: 0.0,
            server_time: None,
            camera,
            wifi_access_point: None,
            demo_image_names: self.demo_images.clone(),
        };
        Self::update_server_information(self.state.clone(), &mut server_info).await;
        server_info
    }

    async fn update_server_information(state: Arc<tokio::sync::Mutex<CedarState>>,
                                       server_info: &mut ServerInformation) {
        if let Some(wifi) = &state.lock().await.wifi {
            let locked_wifi = wifi.lock().unwrap();
            server_info.wifi_access_point = Some(WiFiAccessPoint{
                ssid: Some(locked_wifi.ssid()),
                psk: Some(locked_wifi.psk()),
                channel: Some(locked_wifi.channel())});
        }
        let temp_str =
            fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").unwrap();
        server_info.cpu_temperature = temp_str.trim().parse::<f32>().unwrap() / 1000.0;
        server_info.server_time = Some(prost_types::Timestamp::try_from(
            SystemTime::now()).unwrap());
    }

    fn fill_in_time(fixed_settings: &mut FixedSettings) {
        if let Ok(cur_time) = clock_gettime(ClockId::CLOCK_REALTIME) {
            let mut pst = prost_types::Timestamp::default();
            pst.seconds = cur_time.tv_sec();
            pst.nanos = cur_time.tv_nsec() as i32;
            fixed_settings.current_time = Some(pst);
        }
    }

    async fn set_update_interval(state: &CedarState, update_interval: std::time::Duration)
                                 -> Result<(), CanonicalError> {
        if let Some(attached_camera) = &state.attached_camera {
            attached_camera.lock().await.set_update_interval(update_interval).unwrap();
        }
        state.camera.lock().await.set_update_interval(update_interval)
    }

    async fn reset_session_stats(state: &mut CedarState) {
        state.detect_engine.lock().await.reset_session_stats();
        state.solve_engine.lock().await.reset_session_stats().await;
        state.serve_latency_stats.reset_session();
        state.overall_latency_stats.reset_session();
    }

    // Called when entering SETUP mode.
    async fn set_pre_calibration_defaults(
        state: &mut CedarState) -> Result<(), CanonicalError>
    {
        let mut locked_camera = state.camera.lock().await;
        locked_camera.set_exposure_duration(state.initial_exposure_duration)?;
        if let Err(e) = locked_camera.set_offset(Offset::new(3)) {
            debug!("Could not set offset: {:?}", e);
        }
        Ok(())
    }

    async fn set_gain(state: &mut CedarState, daylight_mode: bool) {
        let mut locked_camera = state.camera.lock().await;
        let gain = if daylight_mode {
            Gain::new(0)
        } else {
            locked_camera.optimal_gain()
        };
        locked_camera.set_gain(gain).unwrap();
    }

    // Called when entering OPERATE mode. The bool indicates whether the
    // calibration succeeded; the only error returned is ABORTED if the
    // calibration is canceled.
    async fn calibrate(state: Arc<tokio::sync::Mutex<CedarState>>)
                       -> Result<bool, CanonicalError> {
        let initial_exposure_duration;
        let max_exposure_duration;
        let binning;
        let detection_sigma;
        let star_count_goal;
        let camera;
        let calibrator;
        let cancel_calibration;
        let calibration_data;
        let detect_engine;
        let solve_engine;
        let solver;
        {
            let locked_state = state.lock().await;
            camera = locked_state.camera.clone();
            calibrator = locked_state.calibrator.clone();
            cancel_calibration = locked_state.cancel_calibration.clone();
            calibration_data = locked_state.calibration_data.clone();
            detect_engine = locked_state.detect_engine.clone();
            solve_engine = locked_state.solve_engine.clone();
            solver = locked_state.solver.clone();
            initial_exposure_duration = locked_state.initial_exposure_duration;
            max_exposure_duration = std::time::Duration::try_from(
                locked_state.fixed_settings.lock().unwrap()
                    .max_exposure_time.clone().unwrap()).unwrap();
            {
                let locked_camera = camera.lock().await;
                let (width, height) = locked_camera.dimensions();
                let _display_sampling;
                (binning, _display_sampling) =
                    Self::compute_binning(&locked_state, width as u32, height as u32);
            }
            // For calibrations, use statically configured sigma value.
            let locked_detect_engine = detect_engine.lock().await;
            detection_sigma = locked_detect_engine.get_detection_sigma();
            star_count_goal = locked_detect_engine.get_star_count_goal();
        }
        let offset = match calibrator.lock().await.calibrate_offset(
            cancel_calibration.clone()).await
        {
            Ok(o) => o,
            Err(e) => {
                if e.code == CanonicalErrorCode::Aborted {
                    return Err(e);
                }
                if e.code != CanonicalErrorCode::Unimplemented {
                    warn!{"Error while calibrating offset: {:?}, using 3", e};
                }
                Offset::new(3)  // Sane fallback value.
            }
        };
        calibration_data.lock().await.camera_offset = Some(offset.value());

        let exp_duration = match calibrator.lock().await.calibrate_exposure_duration(
            initial_exposure_duration, max_exposure_duration, star_count_goal,
            binning, detection_sigma,
            cancel_calibration.clone()).await {
            Ok(ed) => ed,
            Err(e) => {
                if e.code == CanonicalErrorCode::Aborted {
                    return Err(e);
                }
                warn!{"Error while calibrating exposure duration: {:?}, using {:?}",
                      e, initial_exposure_duration};
                return Ok(false);
            }
        };
        calibration_data.lock().await.target_exposure_time =
            Some(prost_types::Duration::try_from(exp_duration).unwrap());
        detect_engine.lock().await.set_calibrated_exposure_duration(Some(exp_duration));

        match calibrator.lock().await.calibrate_optical(
            solver.clone(), binning, detection_sigma,
            cancel_calibration.clone()).await
        {
            Ok((fov, distortion, match_max_error, solve_duration)) => {
                let mut locked_calibration_data = calibration_data.lock().await;
                locked_calibration_data.fov_horizontal = Some(fov);
                locked_calibration_data.lens_distortion = Some(distortion);
                locked_calibration_data.match_max_error = Some(match_max_error);
                let sensor_width_mm = camera.lock().await.sensor_size().0 as f64;
                let lens_fl_mm =
                    sensor_width_mm / (2.0 * (fov / 2.0).to_radians()).tan();
                locked_calibration_data.lens_fl_mm = Some(lens_fl_mm);
                let pixel_width_mm =
                    sensor_width_mm / camera.lock().await.dimensions().0 as f64;
                locked_calibration_data.pixel_angular_size =
                    Some((pixel_width_mm / lens_fl_mm).atan().to_degrees());

                let operation_solve_timeout =
                    std::cmp::min(
                        std::cmp::max(solve_duration * 10, Duration::from_millis(500)),
                        Duration::from_secs(1));  // TODO: max solve time cmd line arg
                let mut locked_solve_engine = solve_engine.lock().await;
                locked_solve_engine.set_fov_estimate(Some(fov)).await?;
                locked_solve_engine.set_distortion(distortion).await?;
                locked_solve_engine.set_match_max_error(match_max_error).await?;
                locked_solve_engine.set_solve_timeout(operation_solve_timeout).await?;
            },
            Err(e) => {
                let mut locked_calibration_data = calibration_data.lock().await;
                locked_calibration_data.fov_horizontal = None;
                locked_calibration_data.lens_distortion = None;
                locked_calibration_data.match_max_error = None;
                locked_calibration_data.lens_fl_mm = None;
                locked_calibration_data.pixel_angular_size = None;
                let mut locked_solve_engine = solve_engine.lock().await;
                locked_solve_engine.set_fov_estimate(None).await?;
                locked_solve_engine.set_distortion(0.0).await?;
                locked_solve_engine.set_match_max_error(0.005).await?;
                // TODO: pass this in? Should come from command line, maybe is
                // max solve time.
                locked_solve_engine.set_solve_timeout(Duration::from_secs(1)).await?;
                if e.code == CanonicalErrorCode::Aborted {
                    return Err(e);
                }
                warn!{"Error while calibrating optics: {:?}", e};
                return Ok(false);
            }
        };
        debug!("Calibration result: {:?}", calibration_data.lock().await);
        Ok(true)
    }

    fn jpeg_encode(img: &Arc<GrayImage>) -> Vec::<u8> {
        let (width, height) = img.dimensions();
        let mut jpg_buf = Vec::<u8>::new();
        jpg_buf.reserve((width * height) as usize);
        let mut buffer = Cursor::new(&mut jpg_buf);
        // 75: 40x compression, bad artifacts.
        // 90: 20x compression, mild artifacts.
        // 95: 13x compression, almost no artifacts.
        let mut jpeg_encoder = JpegEncoder::new_with_quality(
            &mut buffer, /*jpeg_quality=*/95);
        jpeg_encoder.encode_image(img.deref()).unwrap();
        jpg_buf
    }

    async fn get_next_frame(state: Arc<tokio::sync::Mutex<CedarState>>,
                            prev_frame_id: Option<i32>, non_blocking: bool,
                            landscape: bool) -> Option<FrameResult> {
        let overall_start_time = Instant::now();

        let mut frame_result = FrameResult {..Default::default()};
        let mut fixed_settings;
        let operating_mode;
        let focus_assist_mode;
        {
            let locked_state = state.lock().await;
            fixed_settings = locked_state.fixed_settings.lock().unwrap().clone();
            operating_mode = locked_state.operation_settings.operating_mode.unwrap();
            focus_assist_mode = locked_state.operation_settings.focus_assist_mode.unwrap();
            // Fill in our current time.
            Self::fill_in_time(&mut fixed_settings);

            if locked_state.calibrating {
                frame_result.calibrating = true;
                let time_spent_calibrating = locked_state.calibration_start.elapsed();
                let mut fraction =
                    time_spent_calibrating.as_secs_f64() /
                    locked_state.calibration_duration_estimate.as_secs_f64();
                if fraction > 1.0 {
                    fraction = 1.0;
                }
                frame_result.calibration_progress = Some(fraction);
		// Return previous calibration data while calibrating.
		frame_result.calibration_data =
		    Some(locked_state.calibration_data.lock().await.clone());

                if let Some(img) = &locked_state.scaled_image {
                    let (scaled_width, scaled_height) = img.dimensions();
                    let jpg_buf = Self::jpeg_encode(&img);
                    let binning_factor = locked_state.scaled_image_binning_factor as i32;
                    let rotation_size_ratio =
                        locked_state.scaled_image_rotation_size_ratio;
                    let image_rectangle = Rectangle{
                        origin_x: 0, origin_y: 0,
                        width: scaled_width as i32 * binning_factor,
                        height: scaled_height as i32 * binning_factor,
                    };
                    frame_result.image = Some(Image{
                        binning_factor,
                        rotation_size_ratio,
                        // Rectangle is always in full resolution coordinates.
                        rectangle: Some(image_rectangle),
                        image_data: jpg_buf,
                    });
                    frame_result.frame_id = locked_state.scaled_image_frame_id;
                    frame_result.fixed_settings = Some(fixed_settings.clone());
                    frame_result.preferences =
                        Some(locked_state.preferences.lock().unwrap().clone());
                    frame_result.operation_settings =
                        Some(locked_state.operation_settings.clone());
                }
                if non_blocking {
                    frame_result.has_result = Some(true);
                }
                return Some(frame_result);
            }  // Calibrating.
        }  // locked_state.

        // TODO: move most of this into a (new) ServeEngine, another pipeline phase.

        // Populated only in OperatingMode::Operate mode and Setup alignment
        // mode.
        let mut plate_solution: Option<PlateSolution> = None;
        let mut plate_solution_proto: Option<PlateSolutionProto> = None;

        let detect_result =
            if operating_mode == OperatingMode::Setup as i32 && focus_assist_mode {
                // TODO: don't hold state.lock() across a blocking call to
                // get_next_result(). Poll it non-blocking. Not urgent, as our
                // calls are currently non-blocking.
                let dr = state.lock().await.detect_engine.lock().await.
                    get_next_result(prev_frame_id, non_blocking).await;
                if dr.is_none() {
                    return None;
                }

                dr.unwrap()
            } else {
                // TODO: don't hold state.lock() across a blocking call to
                // get_next_result(). Poll it non-blocking. Not urgent, as our
                // calls are currently non-blocking.
                plate_solution = state.lock().await.solve_engine.lock().await.
                    get_next_result(prev_frame_id, non_blocking).await;
                if plate_solution.is_none() {
                    return None;
                }
                let psr = plate_solution.as_ref().unwrap();
                plate_solution_proto = psr.plate_solution.clone();

                psr.detect_result.clone()
            };
        let serve_start_time = Instant::now();
        let mut locked_state = state.lock().await;
        let is_color = locked_state.camera.lock().await.is_color();

        frame_result.frame_id = detect_result.frame_id;
        let captured_image = &detect_result.captured_image;
        let (width, height) = captured_image.image.dimensions();
        let image_rectangle = Rectangle{
            origin_x: 0, origin_y: 0,
            width: width as i32, height: height as i32};
        frame_result.exposure_time = Some(prost_types::Duration::try_from(
            captured_image.capture_params.exposure_duration).unwrap());
        frame_result.capture_time = Some(prost_types::Timestamp::try_from(
            captured_image.readout_time).unwrap());
        frame_result.fixed_settings = Some(fixed_settings.clone());
        frame_result.preferences =
            Some(locked_state.preferences.lock().unwrap().clone());
        frame_result.operation_settings =
            Some(locked_state.operation_settings.clone());

        let daylight_mode = detect_result.daylight_mode;
        frame_result.operation_settings.as_mut().unwrap().daylight_mode =
            Some(daylight_mode);

        let mut centroids = Vec::<StarCentroid>::new();
        for star in &detect_result.star_candidates {
            centroids.push(StarCentroid{
                centroid_position: Some(ImageCoord {
                    x: star.centroid_x, y: star.centroid_y,
                }),
                brightness: star.brightness,
                num_saturated: star.num_saturated as i32,
            });
        }
        frame_result.star_candidates = centroids;
        frame_result.noise_estimate = detect_result.noise_estimate;

        let (binning, display_sampling) =
            Self::compute_binning(&locked_state, width, height);

        if let Some(fa) = &detect_result.focus_aid {
            let ic = ImageCoord {
                x: fa.center_peak_position.0,
                y: fa.center_peak_position.1,
            };
            *locked_state.center_peak_position.lock().unwrap() = Some(ic.clone());
            frame_result.center_peak_position = Some(ic);
            frame_result.center_peak_value = Some(fa.center_peak_value as i32);

            // Populate `center_peak_image`.
            let center_peak_image = &fa.peak_image;
            let peak_image_region = &fa.peak_image_region;
            // center_peak_image_image is taken from the camera's full
            // resolution acquired image. If it is a color camera, we 2x2 bin it
            // to avoid displaying the Bayer grid.
            let binning_factor;
            let center_peak_jpg_buf;
            if is_color {
                let binned_center_peak_image = bin_2x2(center_peak_image.clone());
                binning_factor = 2;
                center_peak_jpg_buf = Self::jpeg_encode(&Arc::new(binned_center_peak_image));
            } else {
                binning_factor = 1;
                center_peak_jpg_buf = Self::jpeg_encode(&Arc::new(center_peak_image.clone()));
            }
            frame_result.center_peak_image = Some(Image{
                binning_factor,
                rotation_size_ratio: 1.0,  // Is not rotated.
                rectangle: Some(Rectangle{
                    origin_x: peak_image_region.left(),
                    origin_y: peak_image_region.top(),
                    width: peak_image_region.width() as i32,
                    height: peak_image_region.height() as i32,
                }),
                image_data: center_peak_jpg_buf,
            });
        } else {
            *locked_state.center_peak_position.lock().unwrap() = None;
        }

        // Populate `image` as requested.
        let mut disp_image = &captured_image.image;
        let mut resized_disp_image = disp_image;
        let mut resize_result: Arc<GrayImage>;
        let mut black_level = detect_result.display_black_level;
        let mut peak_value = detect_result.peak_value;

        if detect_result.binned_image.is_some() {
            disp_image = detect_result.binned_image.as_ref().unwrap();
            resized_disp_image = disp_image;
        } else if binning > 1 {
            // This can happen when we're transitioning away from daylight
            // mode, wherein detect engine is skipping Cedar detect and
            // thus not creating a binned image.
            resize_result = Arc::new(sample_2x2(disp_image.deref().clone()));
            resized_disp_image = &resize_result;
            if binning == 4 {
                resize_result = Arc::new(sample_2x2(resize_result.deref().clone()));
                resized_disp_image = &resize_result;
            }
        }
        if display_sampling {
            resize_result = Arc::new(sample_2x2(resized_disp_image.deref().clone()));
            resized_disp_image = &resize_result;
            // Adjust peak_value, binning can make point sources dimmer in the
            // result.
            peak_value /= 4;
        }
        if black_level > peak_value {
            black_level = peak_value;
        }
        let binning_factor = binning * if display_sampling { 2 } else { 1 };

        if let Some(ref psp) = plate_solution_proto {
            let celestial_coords =
                if psp.target_sky_coord.len() > 0 {
                    psp.target_sky_coord[0].clone()
                } else {
                    psp.image_sky_coord.as_ref().unwrap().clone()
                };
            let bs_ra = celestial_coords.ra.to_radians();
            let bs_dec = celestial_coords.dec.to_radians();
            if fixed_settings.observer_location.is_some() {
                let geo_location = fixed_settings.observer_location.clone().unwrap();
                let lat = geo_location.latitude.to_radians();
                let long = geo_location.longitude.to_radians();
                let time = captured_image.readout_time;
                // alt/az of boresight. Also boresight hour angle.
                let (bs_alt, bs_az, bs_ha) =
                    alt_az_from_equatorial(bs_ra, bs_dec, lat, long, time);
                // ra/dec of zenith.
                let (z_ra, z_dec) = equatorial_from_alt_az(
                    90_f64.to_radians(),
                    0.0,
                    lat, long, time);
                let mut zenith_roll_angle = (position_angle(
                    bs_ra, bs_dec, z_ra, z_dec).to_degrees() + psp.roll) % 360.0;
                // Arrange for angle to be 0..360.
                if zenith_roll_angle < 0.0 {
                    zenith_roll_angle += 360.0;
                }
                frame_result.location_based_info =
                    Some(LocationBasedInfo{zenith_roll_angle,
                                           altitude: bs_alt.to_degrees(),
                                           azimuth: bs_az.to_degrees(),
                                           hour_angle: bs_ha.to_degrees(),
                    });
            }
        }

        locked_state.scaled_image_binning_factor = binning_factor;
        locked_state.scaled_image_rotation_size_ratio = 1.0;

        // Outside of focus mode or daylight align mode, we rotate
        // resized_disp_image to orient zenith up.
        let mut image_rotator: Option<ImageRotator> = None;
        if detect_result.focus_aid.is_none() && !daylight_mode
        {
            // See if frame_result has location_based_info, pick up zenith
            // angle.
            if let Some(ref mut lbi) = frame_result.location_based_info {
                let zenith_roll_angle = lbi.zenith_roll_angle;
                let image_rotate_angle =
                    if landscape {
                        -zenith_roll_angle
                    } else {
                        90.0 - zenith_roll_angle
                    };
                // Adjust reported roll angles for image rotation.
                lbi.zenith_roll_angle += image_rotate_angle;
                // Result is 0 or 90, no need to adjust for mod 360.
                if let Some(psp) = &mut plate_solution_proto {
                    psp.roll = (psp.roll + image_rotate_angle) % 360.0;
                    // Arrange for angle to be 0..360.
                    if psp.roll < 0.0 {
                        psp.roll += 360.0;
                    }
                }
                locked_state.image_rotator =
                    Some(ImageRotator::new(width, height, image_rotate_angle));
            } else {
                // Use previous ImageRotator, if any.
            }
            image_rotator = locked_state.image_rotator.clone();
            if let Some(ref irr) = image_rotator {
                resize_result = Arc::new(irr.rotate_image(&resized_disp_image, /*fill=*/0));
                resized_disp_image = &resize_result;
                locked_state.scaled_image_rotation_size_ratio = irr.size_ratio();
            }
        }

        let gamma = if daylight_mode { 1.0 } else { 0.7 };
        let scaled_image = scale_image(
            resized_disp_image, black_level, peak_value, gamma);
        // Save most recent display image.
        locked_state.scaled_image = Some(Arc::new(scaled_image.clone()));
        let jpg_buf = Self::jpeg_encode(&Arc::new(scaled_image.clone()));

        locked_state.scaled_image_frame_id = frame_result.frame_id;
        frame_result.image = Some(Image{
            binning_factor: binning_factor as i32,
            rotation_size_ratio: locked_state.scaled_image_rotation_size_ratio,
            // Rectangle is always in full resolution coordinates.
            rectangle: Some(image_rectangle),
            image_data: jpg_buf,
        });

        locked_state.serve_latency_stats.add_value(
            serve_start_time.elapsed().as_secs_f64());
        locked_state.overall_latency_stats.add_value(
            overall_start_time.elapsed().as_secs_f64());

        frame_result.processing_stats =
            Some(ProcessingStats{..Default::default()});
        let stats = &mut frame_result.processing_stats.as_mut().unwrap();
        stats.detect_latency = Some(detect_result.detect_latency_stats);
        stats.serve_latency =
            Some(locked_state.serve_latency_stats.value_stats.clone());
        stats.overall_latency =
            Some(locked_state.overall_latency_stats.value_stats.clone());
        if let Some(mut psr) = plate_solution {
            stats.solve_latency = Some(psr.solve_latency_stats.clone());
            stats.solve_attempt_fraction =
                Some(psr.solve_attempt_stats.clone());
            stats.solve_success_fraction =
                Some(psr.solve_success_stats.clone());
            frame_result.slew_request = psr.slew_request.clone();
            if let Some(boresight_image) = &psr.boresight_image {
                // boresight_image is taken from the camera's acquired image. In
                // OPERATE mode the camera capture is always full resolution. If
                // it is a color camera, we 2x2 bin it to avoid displaying the
                // Bayer grid.
                let (binning_factor, resized_boresight_image) =
                    if is_color {
                        (2, bin_2x2(boresight_image.clone()))
                    } else {
                        (1, boresight_image.clone())
                    };

                let rotated_boresight_image;
                let boresight_rotation_size_ratio;
                if let Some(ref irr) = image_rotator {
                    let bsi_rotator =
                        ImageRotator::new(resized_boresight_image.width(),
                                          resized_boresight_image.height(),
                                          irr.angle());
                    rotated_boresight_image =
                        bsi_rotator.rotate_image(&resized_boresight_image, /*fill=*/0);
                    boresight_rotation_size_ratio = bsi_rotator.size_ratio();
                } else {
                    rotated_boresight_image = resized_boresight_image.clone();
                    boresight_rotation_size_ratio = 1.0;
                }

                let jpg_buf =
                    Self::jpeg_encode(&Arc::new(rotated_boresight_image.clone()));

                let bsi_rect = psr.boresight_image_region.unwrap();
                frame_result.boresight_image = Some(Image{
                    binning_factor,
                    rotation_size_ratio: boresight_rotation_size_ratio,
                    // Rectangle is always in full resolution coordinates.
                    rectangle: Some(Rectangle{origin_x: bsi_rect.left(),
                                              origin_y: bsi_rect.top(),
                                              width: bsi_rect.width() as i32,
                                              height: bsi_rect.height() as i32}),
                    image_data: jpg_buf,
                });
            }  // boresight_image
            if frame_result.slew_request.is_some() && image_rotator.is_some() {
                let slew_request = &mut frame_result.slew_request.as_mut().unwrap();
                let irr = &image_rotator.as_ref().unwrap();
                if slew_request.image_pos.is_some() {
                    // Apply rotator to slew target.
                    let slew_target_image_pos =
                        &mut slew_request.image_pos.as_mut().unwrap();
                    (slew_target_image_pos.x, slew_target_image_pos.y) =
                        irr.transform_to_rotated(
                            slew_target_image_pos.x, slew_target_image_pos.y,
                            width as u32, height as u32);
                }
                if let Some(ta) = slew_request.target_angle {
                    // Apply rotator to slew direction.
                    slew_request.target_angle = Some((ta + irr.angle()) % 360.0);
                }
            }

            // Return catalog objects that are in the field of view.
            if let Some(fces) = &mut psr.fov_catalog_entries {
                frame_result.labeled_catalog_entries =
                    Vec::<FovCatalogEntry>::with_capacity(fces.len());
                for ref mut fce in fces {
                    if let Some(ref irr) = image_rotator {
                        let pos = fce.image_pos.as_mut().unwrap();
                        (pos.x, pos.y) = irr.transform_to_rotated(
                            pos.x, pos.y, width, height);
                    }
                    frame_result.labeled_catalog_entries.push(fce.clone());
                }
            }
            if let Some(decrowded_fces) = &mut psr.decrowded_fov_catalog_entries {
                frame_result.unlabeled_catalog_entries =
                    Vec::<FovCatalogEntry>::with_capacity(decrowded_fces.len());
                for ref mut fce in decrowded_fces {
                    if let Some(ref irr) = image_rotator {
                        let pos = fce.image_pos.as_mut().unwrap();
                        (pos.x, pos.y) = irr.transform_to_rotated(
                            pos.x, pos.y, width, height);
                    }
                    frame_result.unlabeled_catalog_entries.push(fce.clone());
                }
            }
        }
        if let Some(ref psp) = plate_solution_proto {
            frame_result.plate_solution = Some(psp.clone());
            if let Some(ref mut slew_request) = frame_result.slew_request {
                let celestial_coords =
                    if psp.target_sky_coord.len() > 0 {
                        psp.target_sky_coord[0].clone()
                    } else {
                        psp.image_sky_coord.as_ref().unwrap().clone()
                    };
                let bs_ra = celestial_coords.ra.to_radians();
                let bs_dec = celestial_coords.dec.to_radians();

                let target_ra = slew_request.target.as_ref().unwrap().ra;
                let target_dec = slew_request.target.as_ref().unwrap().dec;
                let mount_type = locked_state.preferences.lock().unwrap().mount_type;
                if mount_type == Some(MountType::Equatorial.into()) {
                    // Compute the movement required in RA and Dec to move boresight to
                    // target.
                    let mut rel_ra = target_ra - bs_ra.to_degrees();
                    if rel_ra < -180.0 {
                        rel_ra += 360.0;
                    }
                    if rel_ra > 180.0 {
                        rel_ra -= 360.0;
                    }
                    slew_request.offset_rotation_axis = Some(rel_ra);

                    let rel_dec = target_dec - bs_dec.to_degrees();
                    slew_request.offset_tilt_axis = Some(rel_dec);
                }
                if fixed_settings.observer_location.is_some() &&
                    mount_type == Some(MountType::AltAz.into())
                {
                    // Compute the movement required in azimuith and altitude to move
                    // boresight to target.
                    let geo_location = fixed_settings.observer_location.clone().unwrap();
                    let lat = geo_location.latitude.to_radians();
                    let long = geo_location.longitude.to_radians();
                    let time = captured_image.readout_time;
                    // alt/az of boresight.
                    let (bs_alt, bs_az, _bs_ha) =
                        alt_az_from_equatorial(bs_ra, bs_dec, lat, long, time);
                    // alt/az of target.
                    let (target_alt, target_az, _target_ha) =
                        alt_az_from_equatorial(target_ra.to_radians(),
                                               target_dec.to_radians(),
                                               lat, long, time);
                    let mut rel_az = target_az.to_degrees() - bs_az.to_degrees();
                    if rel_az < -180.0 {
                        rel_az += 360.0;
                    }
                    if rel_az > 180.0 {
                        rel_az -= 360.0;
                    }
                    slew_request.offset_rotation_axis = Some(rel_az);

                    let rel_alt = target_alt.to_degrees() - bs_alt.to_degrees();
                    slew_request.offset_tilt_axis = Some(rel_alt);
                }
            }
        }  // plate_solution_proto.
        let boresight_position =
            locked_state.solve_engine.lock().await.boresight_pixel().await;
        if let Some(bs) = boresight_position {
            frame_result.boresight_position = Some(ImageCoord{x: bs.x, y: bs.y});
        } else {
            frame_result.boresight_position =
                Some(ImageCoord{x: width as f64 / 2.0,
                                y: height as f64 / 2.0});
        }
        if let Some(ref irr) = image_rotator {
            // Having an image_rotator implies that we are not in focus or
            // daytime align mode.

            // Transform the boresight coords.
            let bp = frame_result.boresight_position.as_mut().unwrap();
            (bp.x, bp.y) = irr.transform_to_rotated(bp.x, bp.y, width, height);

            // Setup align mode?
            if operating_mode == OperatingMode::Setup as i32 && !focus_assist_mode {
                // Replace star_candidates with plate solve's catalog stars.
                if let Some(ref psp) = plate_solution_proto {
                    frame_result.star_candidates = Vec::<StarCentroid>::new();
                    for star in &psp.catalog_stars {
                        let ic = star.pixel.clone().unwrap();
                        frame_result.star_candidates.push(
                            StarCentroid{centroid_position: Some(ImageCoord{x: ic.x, y: ic.y}),
                                         // Arbitrarily assign intensity=1 to mag=6.
                                         brightness: magnitude_intensity_ratio(
                                             6.0, star.mag as f64),
                                         num_saturated: 0
                            });
                    }
                }
            }

            // Transform the detected (or plate solve catalog) star image coordinates.
            for star_centroid in &mut frame_result.star_candidates {
                let cp = star_centroid.centroid_position.as_mut().unwrap();
                (cp.x, cp.y) = irr.transform_to_rotated(
                    cp.x, cp.y, width, height);
            }

            // Setup align mode?
            if operating_mode == OperatingMode::Setup as i32 && !focus_assist_mode {
                // Augment the detected stars with catalog items from the plate solution.
                // The labeled_catalog_entries have already been transformed to rotated.
                frame_result.star_candidates = fill_in_detections(
                    &frame_result.star_candidates, &frame_result.labeled_catalog_entries);
            }
        }

        frame_result.calibration_data =
            Some(locked_state.calibration_data.lock().await.clone());
        frame_result.polar_align_advice = Some(
            locked_state.polar_analyzer.lock().unwrap().get_polar_align_advice());

        Some(frame_result)
    }

    // MyCedar::new().
    pub async fn new(
        solver: Arc<tokio::sync::Mutex<dyn SolverTrait + Send + Sync>>,
        args_binning: Option<u32>,
        args_display_sampling: Option<bool>,
        invert_camera: bool,
        initial_exposure_duration: Duration,
        min_exposure_duration: Duration,
        mut max_exposure_duration: Duration,
        activity_led: Arc<Mutex<ActivityLed>>,
        attached_camera: Option<Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>>,
        test_image_camera: Option<Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>>,
        camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
        telescope_position: Arc<Mutex<TelescopePosition>>,
        base_star_count_goal: i32,
        base_detection_sigma: f64,
        min_detection_sigma: f64,
        stats_capacity: usize,
        preferences_file: PathBuf,
        log_file: PathBuf,
        product_name: &str,
        copyright: &str,
        feature_level: FeatureLevel,
        cedar_sky: Option<Arc<Mutex<dyn CedarSkyTrait + Send>>>,
        wifi: Option<Arc<Mutex<dyn WifiTrait + Send>>>)
        -> Result<Self, CanonicalError>
    {
        let cedar_version = env!("CARGO_PKG_VERSION");
        let processor_model =
            fs::read_to_string("/sys/firmware/devicetree/base/model").unwrap()
            .trim_end_matches('\0').to_string();
        let serial_number =
            fs::read_to_string("/sys/firmware/devicetree/base/serial-number").unwrap()
            .trim_end_matches('\0').to_string();
        let reader = BufReader::new(fs::File::open("/etc/os-release").unwrap());
        let mut os_version: String = "".to_string();
        for line in reader.lines() {
            let line = line.unwrap();
            if line.starts_with("PRETTY_NAME=") {
                let parts: Vec<&str> = line.split('=').collect();
                os_version = parts[1].trim_matches('"').to_string();
                break;
            }
        }
        info!("{}", &product_name);
        info!("{}", &copyright);
        info!("Cedar server version {} running on {}/{}",
              &cedar_version, &processor_model, &os_version);
        info!("Processor serial number {}", &serial_number);

        let mut normalize_rows = false;
        if let Some(attached_camera) = &attached_camera {
            let locked_camera = attached_camera.lock().await;
            if (locked_camera.model() == "imx296" || locked_camera.model() == "imx290")
                && processor_model.contains("Raspberry Pi Zero 2 W")
            {
                normalize_rows = true;
            }
            if locked_camera.is_color() {
                // Double max exposure time for color camera, which are
                // generally less sensitive than monochrome cameras.
                // max_exposure_duration *= 2;
                max_exposure_duration *= 1;
            }
        }

        let detect_engine = Arc::new(tokio::sync::Mutex::new(DetectEngine::new(
            initial_exposure_duration,
            min_exposure_duration, max_exposure_duration,
            min_detection_sigma, base_detection_sigma,
            base_star_count_goal,
            camera.clone(),
            normalize_rows,
            stats_capacity)));

        // Set up initial Preferences to use if preferences file cannot be loaded.
        let mut preferences = Preferences{
            celestial_coord_format: Some(CelestialCoordFormat::HmsDms.into()),
            eyepiece_fov: Some(1.0),
            night_vision_theme: Some(false),
            hide_app_bar: Some(true),
            mount_type: Some(MountType::AltAz.into()),
            observer_location: None,
            update_interval: match feature_level {
                FeatureLevel::Plus => Some(
                    // 10Hz, max.
                    prost_types::Duration { seconds: 0, nanos: 100000000 }
                ),
                FeatureLevel::Basic => Some(
                    // 3Hz (max 5Hz).
                    prost_types::Duration { seconds: 0, nanos: 333000000 }
                ),
                _ => Some(
                    // DIY: Unlimited.
                    prost_types::Duration { seconds: 0, nanos: 0 }
                ),
            },
            catalog_entry_match: if cedar_sky.is_some() {
                let mut cat_match =
                    Some(CatalogEntryMatch {
                        faintest_magnitude: match feature_level {
                            FeatureLevel::Plus => Some(12),  // Max 20.
                            FeatureLevel::Basic => Some(8),  // Max 12.
                            _ => Some(10),  // Irrelevant, no Cedar Sky.
                        },
                        match_catalog_label: true,
                        catalog_label: Vec::<String>::new(),  // Filled below.
                        match_object_type_label: true,
                        object_type_label: Vec::<String>::new(),  // Filled below.
                    });
                let cm_ref = cat_match.as_mut().unwrap();
                // All catalog labels.
                cm_ref.catalog_label = vec![
                    "M".to_string(), "NGC".to_string(), "IC".to_string(),
                    "IAU".to_string(), "PL".to_string()];
                // All object types.
                cm_ref.object_type_label = vec![
                    "star".to_string(), "double star".to_string(),
                    "star association".to_string(),
                    "open cluster".to_string(), "globular cluster".to_string(),
                    "star cluster + nebula".to_string(),
                    "galaxy".to_string(), "galaxy pair".to_string(),
                    "galaxy triplet".to_string(), "galaxy group".to_string(),
                    "planetary nebula".to_string(), "HII ionized region".to_string(),
                    "dark nebula".to_string(), "emission nebula".to_string(),
                    "nebula".to_string(), "reflection nebula".to_string(),
                    "supernova remnant".to_string(), "nova star".to_string(),
                    "planet".to_string(), "dwarf planet".to_string()];
                cat_match
            } else {
                None  // No Cedar sky.
            },
            max_distance_active: Some(false),
            max_distance: Some(60.0),
            min_elevation_active: Some(true),
            min_elevation: Some(20.0),
            ordering: Some(Ordering::Brightness.into()),
            advanced: Some(false),
            text_size_index: Some(0),
            boresight_pixel: None,
            invert_camera: Some(invert_camera),  // Initial value from command line.
            right_handed: Some(true),
            celestial_coord_choice: Some(CelestialCoordChoice::RaDec.into()),
            screen_always_on: Some(true),
        };

        // If there is a preferences file, read it and merge its contents into
        // initial `preferences`. Fields that are present in the both
        // `preferences` and the preferences file will replace those in initial
        // `preferences`.

        // Load UI preferences file.
        let prefs_path = Path::new(&preferences_file);
        let file_prefs_bytes = fs::read(prefs_path);
        if let Err(e) = file_prefs_bytes {
            warn!("Could not read file {:?}: {:?}", preferences_file, e);
        } else {
            match Preferences::decode(file_prefs_bytes.as_ref().unwrap().as_slice()) {
                Ok(mut file_prefs) => {
                    if let Some(fov) = file_prefs.eyepiece_fov {
                        if fov < 0.1 {
                            file_prefs.eyepiece_fov = Some(0.1);
                        }
                        if fov > 2.0 {
                            file_prefs.eyepiece_fov = Some(2.0);
                        }
                    }
                    if file_prefs.catalog_entry_match.is_some() {
                        // The protobuf merge() function accumulates into
                        // repeated fields of the destination; we don't want
                        // this.
                        preferences.catalog_entry_match = None;
                    }
                    if file_prefs.update_interval.is_some() {
                        preferences.update_interval = None;
                    }
                    preferences.merge(&*file_prefs_bytes.unwrap()).unwrap();
                },
                Err(e) => {
                    warn!("Could not decode preferences {:?}", e);
                },
            }
        }
        let (width, height) = camera.lock().await.dimensions();
        let inset = 16;
        if let Some(ref bsp) = preferences.boresight_pixel {
            // Validate boresight_pixel loaded from preferences, to make sure it
            // is within the image area. This could be violated if e.g. we
            // changed camera since the preferences were saved.
            if bsp.x < inset as f64 || bsp.x > (width - inset) as f64 ||
                bsp.y < inset as f64 || bsp.y > (height - inset) as f64
            {
                preferences.boresight_pixel = None;
            }
        }
        // Validate preferences against feature level. If someone switches the
        // camera down to the basic model, some preferences need to be adjusted.
        let (limit_magnitude, min_interval_nanos) = match feature_level {
            FeatureLevel::Plus => (20, 100000000),  // 100ms, or 10Hz.
            FeatureLevel::Basic => (12, 200000000),  // 200ms, or 5Hz.
            _ => (20, 0),  // DIY.
        };
        if let Some(ref ui) = preferences.update_interval {
            if ui.seconds == 0 && ui.nanos < min_interval_nanos {
                preferences.update_interval.as_mut().unwrap().nanos = min_interval_nanos;
            }
        }
        if let Some(ref cm) = preferences.catalog_entry_match {
            if cm.faintest_magnitude.unwrap() > limit_magnitude {
                preferences.catalog_entry_match.as_mut().unwrap().faintest_magnitude =
                    Some(limit_magnitude);
            }
        }
        if feature_level == FeatureLevel::Basic {
            preferences.mount_type = Some(MountType::AltAz.into());
        }

        let shared_preferences = Arc::new(Mutex::new(preferences));

        let fixed_settings = Arc::new(Mutex::new(FixedSettings {
            observer_location: shared_preferences.lock().unwrap().observer_location.clone(),
            current_time: None,
            session_name: None,
            max_exposure_time: Some(
                prost_types::Duration::try_from(max_exposure_duration).unwrap()),
        }));

        let polar_analyzer = Arc::new(Mutex::new(PolarAnalyzer::new()));

        // Define callback invoked from SolveEngine().
        let closure_fixed_settings = fixed_settings.clone();
        let closure_preferences = shared_preferences.clone();
        let closure_preferences_file = preferences_file.clone();
        let closure_telescope_position = telescope_position.clone();
        let motion_estimator = Arc::new(Mutex::new(MotionEstimator::new(
            /*gap_tolerance=*/Duration::from_secs(3),
            /*bump_tolerance=*/Duration::from_secs_f64(2.0))));
        let closure_polar_analyzer = polar_analyzer.clone();
        let closure = Arc::new(move |boresight_pixel: Option<ImageCoord>,
                                     detect_result: Option<DetectResult>,
                                     plate_solution: Option<PlateSolutionProto>|
        {
            Self::solution_callback(
                boresight_pixel,
                detect_result,
                plate_solution,
                closure_fixed_settings.clone(),
                closure_preferences.clone(),
                closure_preferences_file.clone(),
                closure_telescope_position.clone(),
                motion_estimator.clone(),
                closure_polar_analyzer.clone())
        });
        let state =
        {
            let locked_preferences = shared_preferences.lock().unwrap();
            Arc::new(tokio::sync::Mutex::new(CedarState {
                solver: solver.clone(),
                attached_camera: attached_camera.clone(),
                camera: camera.clone(),
                initial_exposure_duration,
                fixed_settings,
                operation_settings: OperationSettings {
                    operating_mode: Some(OperatingMode::Setup as i32),
                    daylight_mode: Some(false),
                    focus_assist_mode: Some(true),
                    update_interval: locked_preferences.update_interval.clone(),
                    dwell_update_interval: None,
                    log_dwelled_positions: Some(false),
                    catalog_entry_match: locked_preferences.catalog_entry_match.clone(),
                    demo_image_filename: None,
                    invert_camera: locked_preferences.invert_camera,
                },
                calibration_data: Arc::new(tokio::sync::Mutex::new(
                    CalibrationData{..Default::default()})),
                detect_engine: detect_engine.clone(),
                solve_engine: Arc::new(tokio::sync::Mutex::new(SolveEngine::new(
                    normalize_rows,
                    solver.clone(), cedar_sky.clone(), detect_engine.clone(),
                    stats_capacity, closure).await.unwrap())),
                calibrator: Arc::new(tokio::sync::Mutex::new(
                    Calibrator::new(camera.clone(), normalize_rows))),
                telescope_position,
                polar_analyzer,
                activity_led,
                cedar_sky, wifi,
                preferences: shared_preferences.clone(),
                scaled_image: None,
                scaled_image_binning_factor: 1,
                scaled_image_rotation_size_ratio: 1.0,
                scaled_image_frame_id: 0,
                image_rotator: None,
                calibrating: false,
                cancel_calibration: Arc::new(Mutex::new(false)),
                calibration_start: Instant::now(),
                calibration_duration_estimate: Duration::MAX,
                center_peak_position: Arc::new(Mutex::new(None)),
                serve_latency_stats: ValueStatsAccumulator::new(stats_capacity),
                overall_latency_stats: ValueStatsAccumulator::new(stats_capacity),
                args_binning, args_display_sampling,
            }))
        };

        let mut demo_images: Vec<String> = vec![];
        match Self::get_demo_images() {
            Ok(d) => {
                demo_images = d;
            },
            Err(x) => {
                warn!("Could not enumerate demo images {:?}", x);
            }
        }

        let cedar = MyCedar {
            state: state.clone(),
            test_image_camera: test_image_camera.clone(),
            demo_images,
            preferences_file,
            log_file,
            product_name: product_name.to_string(),
            copyright: copyright.to_string(),
            feature_level,
            cedar_version: cedar_version.to_string(),
            processor_model,
            os_version,
            serial_number,
        };
        // Set pre-calibration defaults on camera.
        let mut locked_state = state.lock().await;
        let (width, height) = locked_state.camera.lock().await.dimensions();
        let (binning, display_sampling) =
            Self::compute_binning(&locked_state, width as u32, height as u32);

        if let Err(x) = Self::set_pre_calibration_defaults(&mut locked_state).await
        {
            warn!("Could not set default settings on camera {:?}", x);
        }

        locked_state.detect_engine.lock().await.set_binning(binning, display_sampling);
        locked_state.detect_engine.lock().await.set_focus_mode(
            locked_state.operation_settings.focus_assist_mode.unwrap());
        locked_state.detect_engine.lock().await.set_daylight_mode(
            locked_state.operation_settings.daylight_mode.unwrap());
        locked_state.solve_engine.lock().await.set_catalog_entry_match(
            shared_preferences.lock().unwrap().catalog_entry_match.clone()).await;
        locked_state.solve_engine.lock().await.set_align_mode(true).await;
        if let Some(bsp) = &shared_preferences.lock().unwrap().boresight_pixel {
            locked_state.solve_engine.lock().await.set_boresight_pixel(
                Some(ImageCoord{x: bsp.x, y: bsp.y})).await.unwrap();
        }

        Ok(cedar)
    }  // MyCedar::new().

    // From Gemini.
    fn find_most_recent_file(pattern: &str) -> Option<PathBuf> {
        let mut latest_file: Option<(PathBuf, u64)> = None;

        for entry in glob(pattern).expect("Failed to read glob pattern") {
            match entry {
                Ok(path) => {
                    let metadata = metadata(&path).expect("Failed to read metadata");
                    let modified_time = metadata.modified()
                        .expect("Failed to get modified time")
                        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                    if latest_file.is_none() ||
                        modified_time > latest_file.as_ref().unwrap().1
                    {
                        latest_file = Some((path, modified_time));
                    }
                },
                Err(e) => {
                    warn!("Error globbing pattern {:?}: {:?}", pattern, e);
                }
            }
        }

        if let Some(result) = latest_file {
            Some(result.0)
        } else {
            None
        }
    }

    fn read_log_tail(log_file: &PathBuf, bytes_to_read: i32) -> io::Result<String> {
        let pat = log_file.to_str().unwrap().to_owned() + ".*";
        let latest_file = Self::find_most_recent_file(&pat);
        if latest_file.is_none() {
            return Err(io::Error::new(ErrorKind::NotFound,
                                      format!("No match for {:?}", pat)));
        }
        let mut f = fs::File::open(latest_file.unwrap())?;
        let len = f.metadata()?.len();
        let to_read = std::cmp::min(len, bytes_to_read as u64) as i64;
        f.seek(SeekFrom::End(-to_read))?;
        let mut content = String::new();
        f.read_to_string(&mut content)?;
        // Trim leading portion of content until first newline.
        if let Some(pos) = content.find('\n') {
            content = content[pos+1..].to_string();
        }
        Ok(content)
    }

    fn solution_callback(boresight_pixel: Option<ImageCoord>,
                         detect_result: Option<DetectResult>,
                         plate_solution: Option<PlateSolutionProto>,
                         fixed_settings: Arc<Mutex<FixedSettings>>,
                         preferences: Arc<Mutex<Preferences>>,
                         preferences_file: PathBuf,
                         telescope_position: Arc<Mutex<TelescopePosition>>,
                         motion_estimator: Arc<Mutex<MotionEstimator>>,
                         polar_analyzer: Arc<Mutex<PolarAnalyzer>>)
                         -> (Option<CelestialCoord>,
                             Option<CelestialCoord>)
    {
        // Notice when solve engine has recently changed its boresight due
        // to a previous call to this callback function reporting a SkySafari
        // sync.
        let mut prefs_to_save: Option<Preferences> = None;
        if let Some(bp) = boresight_pixel {
            let cedar_bp = ImageCoord{x: bp.x, y: bp.y};
            let mut locked_preferences = preferences.lock().unwrap();
            if locked_preferences.boresight_pixel.is_none() ||
                cedar_bp != *locked_preferences.boresight_pixel.as_ref().unwrap()
            {
                // Save in preferences.
                locked_preferences.boresight_pixel = Some(cedar_bp);
                // Flag updated preferences to write to file below.
                prefs_to_save = Some(locked_preferences.clone());
            }
        }
        let mut sync_coord: Option<CelestialCoord> = None;
        if plate_solution.is_none() {
            telescope_position.lock().unwrap().boresight_valid = false;
            if let Some(detect_result) = detect_result {
                motion_estimator.lock().unwrap().add(
                    detect_result.captured_image.readout_time,
                    None, None);
            }
        } else {
            let plate_solution = plate_solution.unwrap();
            // Update SkySafari telescope interface with our position.
            let coords =
                if plate_solution.target_sky_coord.len() > 0 {
                    plate_solution.target_sky_coord[0].clone()
                } else {
                    plate_solution.image_sky_coord.as_ref().unwrap().clone()
                };
            let mut locked_telescope_position = telescope_position.lock().unwrap();
            locked_telescope_position.boresight_ra = coords.ra;
            locked_telescope_position.boresight_dec = coords.dec;
            locked_telescope_position.boresight_valid = true;
            let readout_time = detect_result.unwrap().captured_image.readout_time;
            motion_estimator.lock().unwrap().add(
                readout_time, Some(coords.clone()), Some(plate_solution.rmse));

            // Has SkySafari reported the site geolocation?
            if locked_telescope_position.site_latitude.is_some() &&
                locked_telescope_position.site_longitude.is_some()
            {
                let observer_location = LatLong{
                    latitude: locked_telescope_position.site_latitude.unwrap(),
                    longitude: locked_telescope_position.site_longitude.unwrap(),
                };
                fixed_settings.lock().unwrap().observer_location =
                    Some(observer_location.clone());
                info!("Alpaca updated observer location to {:?}", observer_location);
                locked_telescope_position.site_latitude = None;
                locked_telescope_position.site_longitude = None;
                // Save in preferences.
                let mut locked_preferences = preferences.lock().unwrap();
                locked_preferences.observer_location = Some(observer_location.clone());
                // Flag updated preferences to write to file below.
                prefs_to_save = Some(locked_preferences.clone());
            }
            // Has SkySafari done a "sync"?
            if locked_telescope_position.sync_ra.is_some() &&
                locked_telescope_position.sync_dec.is_some()
            {
                sync_coord = Some(CelestialCoord{
                    ra: locked_telescope_position.sync_ra.unwrap(),
                    dec: locked_telescope_position.sync_dec.unwrap()});
                info!("Alpaca synced boresight to {:?}", sync_coord);
                locked_telescope_position.sync_ra = None;
                locked_telescope_position.sync_dec = None;
            }

            let geo_location = &fixed_settings.lock().unwrap().observer_location;
            if let Some(geo_location) = geo_location {
                let lat = geo_location.latitude.to_radians();
                let long = geo_location.longitude.to_radians();
                let bs_ra = coords.ra.to_radians();
                let bs_dec = coords.dec.to_radians();
                // alt/az of boresight. Also boresight hour angle.
                let (_alt, _az, ha) =
                    alt_az_from_equatorial(bs_ra, bs_dec, lat, long, readout_time);
                let motion_estimate = motion_estimator.lock().unwrap().get_estimate();
                polar_analyzer.lock().unwrap().process_solution(
                    &coords,
                    ha.to_degrees(),
                    geo_location.latitude,
                    &motion_estimate);
            }
        }
        if let Some(prefs) = prefs_to_save {
            // Write updated preferences to file.
            Self::write_preferences_file(&preferences_file, &prefs);
        }
        let locked_telescope_position = telescope_position.lock().unwrap();
        if locked_telescope_position.slew_active {
            (Some(CelestialCoord{
                ra: locked_telescope_position.slew_target_ra,
                dec: locked_telescope_position.slew_target_dec}),
             sync_coord)
        } else {
            (None, sync_coord)
        }
    }
}  // impl MyCedar.

// About Resolutions
//
// Cedar is designed to support a wide variety of cameras. It has been extensively
// tested with two rather different camera sensors:
//
// ASI120mm mini (AR0130CS):         1.2 megapixel, mono,  6.0 mm diagonal
// Raspberry Pi HQ camera (IMX477): 12.3 megapixel, color, 7.9 mm diagonal
//
// Cedar works very well with low resolution cameras such as the ASI mini. The
// high pixel resolution of the HQ camera presents some challenges:
//
// * Star images are typically highly oversampled (spread out over many pixels)
//   and often exceed CedarDetect's star profile shape window, and thus are not
//   detected.
// * The HQ image has too-high resolution for the CedarAim phone UI. A half
//   megapixel or less is adequate for good UI rendering.
// * Sending the HQ image to the phone UI takes too long.
//
// We thus employ image downsizing at various points in the processing chain.
//
// For the HQ camera, we employ 4x4 binning in order to fit CedarDetect's star
// profile shape window. Note that star candidates are then referenced to the
// full-resolution original capture for high-accuracy centroiding.
//
// An additional 2x2 sampling is used when sending HQ images to the CedarAim
// phone UI. For the HQ camera, the result is a 0.2 megapixel display image
// (around 500x375), which is adequate to provide a background for visualizing
// the plate solve result (this can be overridden with a command line flag e.g.
// for a tablet UI; see below).
//
// For the ASI mini camera, we apply 2x2 binning prior to CedarDetect, and refer
// star detections to the full resolution capture for centroiding. The binned
// image (0.3 megapixel) is sent to the phone UI.
//
// Rather than hardwiring the above image downsizing strategies for the HQ
// camera and the ASI mini camera, we instead generalize based on the camera
// sensor resolution:
//
// Camera  Mpix     CedarDetect    Display
//         <0.75
// ASI     0.75-3   2x2 binning
//         3-12     4x4 binning
// HQ      >12      4x4 binning    +2x2 sampling

// Note that the "display" sampling value is an additional sampling (if any)
// applied after the CedarDetect binning has been applied.
//
// Command line arguments are provided to allow overrides to be applied to the
// above rubric.

#[derive(Debug)]
struct AppArgs {
    tetra3_script: String,
    tetra3_database: String,
    camera_interface: String,
    camera_index: i32,
    binning: Option<u32>,
    display_sampling: Option<bool>,
    test_image: Option<String>,
    min_exposure: Duration,
    max_exposure: Duration,
    star_count_goal: i32,
    sigma: f64,
    min_sigma: f64,
    ui_prefs: String,
    log_dir: String,
    log_file: String,
    // TODO: max solve time
}

fn parse_duration(arg: &str)
                  -> Result<std::time::Duration, std::num::ParseFloatError> {
    let seconds = arg.parse()?;
    Ok(std::time::Duration::from_secs_f64(seconds))
}

// `invert_camera` Determines whether camera image is inverted (rot180) during
//     readout.
// `get_dependencies` Is called to obtain the CedarSkyTrait and WifiTrait
//     implementations, if any. This function is called after logging has been
//     set up and `server_main()`s command line arguments have been consumed.
//     The AtomicBool is set to true if control-c occurs.
pub fn server_main(
    product_name: &str, copyright: &str,
    flutter_app_path: &str,
    invert_camera: bool,
    get_dependencies: fn(Arguments)
                         -> (Option<Arc<Mutex<dyn CedarSkyTrait + Send>>>,
                             Option<Arc<Mutex<dyn WifiTrait + Send>>>,
                             Option<Arc<tokio::sync::Mutex<
                                     dyn SolverTrait + Send + Sync>>>)) {
    const HELP: &str = "\
    FLAGS:
      -h, --help                     Prints help information

    OPTIONS:
      --tetra3_script <path>         ../cedar/tetra3_server/python/tetra3_server.py
      --tetra3_database <name>       default_database
      --camera_interface asi|rpi
      --camera_index NUMBER
      --binning 1|2|4
      --display_sampling true|false
      --test_image <path>
      --min_exposure NUMBER          0.00001
      --max_exposure NUMBER          1.0
      --star_count_goal NUMBER       20
      --sigma NUMBER                 8.0
      --min_sigma NUMBER             5.0
      --ui_prefs <path>              ./cedar_ui_prefs.binpb
      --log_dir <path>               .
      --log_file <file>              cedar_log.txt
    ";

    let mut pargs = Arguments::from_env();
    if pargs.contains(["-h", "--help"]) {
        println!("{}", HELP);
        std::process::exit(0);
    }
    let args = AppArgs {
        tetra3_script: pargs.value_from_str("--tetra3_script").
            unwrap_or("../cedar/tetra3_server/python/tetra3_server.py".to_string()),
        tetra3_database: pargs.value_from_str("--tetra3_database").
            unwrap_or("default_database".to_string()),
        camera_interface: pargs.value_from_str("--camera_interface").
            unwrap_or("".to_string()),
        camera_index: pargs.value_from_str("--camera_index").
            unwrap_or(0),
        binning: pargs.opt_value_from_str("--binning").unwrap(),
        display_sampling: pargs.opt_value_from_str("--display_sampling").unwrap(),
        test_image: pargs.opt_value_from_str("--test_image").unwrap(),
        min_exposure: pargs.value_from_fn("--min_exposure", parse_duration).
            unwrap_or(parse_duration("0.00001").unwrap()),
        max_exposure: pargs.value_from_fn("--max_exposure", parse_duration).
            unwrap_or(parse_duration("1.0").unwrap()),
        star_count_goal: pargs.value_from_str("--star_count_goal").
            unwrap_or(20),
        sigma: pargs.value_from_str("--sigma").
            unwrap_or(8.0),
        min_sigma: pargs.value_from_str("--min_sigma").
            unwrap_or(5.0),
        ui_prefs: pargs.value_from_str("--ui_prefs").
            unwrap_or("./cedar_ui_prefs.binpb".to_string()),
        log_dir: pargs.value_from_str("--log_dir").
            unwrap_or(".".to_string()),
        log_file: pargs.value_from_str("--log_file").
            unwrap_or("cedar_log.txt".to_string()),
    };

    // Set up logging.
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(&args.log_file)
        .max_log_files(10)
        .build(&args.log_dir).unwrap();

    // Create non-blocking writers for both the file and stdout
    let (non_blocking_file, _guard1) = NonBlockingBuilder::default()
        .lossy(false)
        .finish(file_appender);
    let (non_blocking_stdout, _guard2) = NonBlockingBuilder::default()
        .lossy(false)
        .finish(std::io::stdout());
    let _subscriber = registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(non_blocking_stdout))
        .with(fmt::layer().with_ansi(false).with_writer(non_blocking_file))
        .init();
    let remaining = pargs.finish();

    let got_signal = Arc::new(AtomicBool::new(false));
    let got_signal2 = got_signal.clone();
    ctrlc::set_handler(move || {
        info!("Got control-c");
        got_signal2.store(true, AtomicOrdering::Relaxed);
        std::thread::sleep(Duration::from_secs(1));
        info!("Exiting");
        std::process::exit(-1);
    }).unwrap();

    let (cedar_sky, wifi, solver) = get_dependencies(Arguments::from_vec(remaining));
    async_main(args, product_name, copyright, flutter_app_path, invert_camera,
               got_signal, cedar_sky, wifi, solver);
}

fn get_attached_camera(camera_interface: Option<&CameraInterface>,
                       camera_index: i32)
                       -> Result<Box<dyn AbstractCamera + Send>, CanonicalError>
{
    select_camera(camera_interface, camera_index)
}

fn get_camera(
    attached_camera: &Option<Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>>,
    test_image_camera: &Option<Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>>)
    -> Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>
{
    if let Some(test_image_camera) = test_image_camera {
        return test_image_camera.clone();
    }
    if let Some(attached_camera) = attached_camera {
        return attached_camera.clone();
    }
    // Fake up a uniform grey ImageCamera.
    let width = 800;
    let height = 600;
    let pixels = vec![16_u8; width * height];
    let img_u8 = GrayImage::from_vec(
        width as u32, height as u32, pixels).unwrap();
    return Arc::new(tokio::sync::Mutex::new(Box::new(
        ImageCamera::new(img_u8).unwrap())));
}

#[tokio::main]
async fn async_main(
    args: AppArgs, product_name: &str, copyright: &str,
    flutter_app_path: &str, invert_camera: bool,
    got_signal: Arc<AtomicBool>,
    cedar_sky: Option<Arc<Mutex<dyn CedarSkyTrait + Send>>>,
    wifi: Option<Arc<Mutex<dyn WifiTrait + Send>>>,
    injected_solver: Option<Arc<tokio::sync::Mutex<dyn SolverTrait + Send + Sync>>>)
{
    // If any thread panics, bail out.
    std::panic::set_hook(Box::new(|panic_info| {
        eprintln!("Thread panicked: {}", panic_info);
        std::process::exit(1);
    }));

    let camera_interface = match args.camera_interface.as_str() {
        "" => None,
        "asi" => Some(CameraInterface::ASI),
        "rpi" => Some(CameraInterface::Rpi),
        _ => {
            error!("Unrecognized 'camera_interface' value: {}", args.camera_interface);
            std::process::exit(1);
        }
    };

    let attached_camera = match get_attached_camera(
        camera_interface.as_ref(), args.camera_index)
    {
        Ok(mut cam) => {
            cam.set_inverted(invert_camera).unwrap();
            Some(Arc::new(tokio::sync::Mutex::new(cam)))
        },
        Err(e) => {
            error!("Could not select camera: {:?}", e);
            None
        }
    };

    let test_image_camera: Option<Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>> =
        match &args.test_image
    {
        Some(test_image_path) => {
            let input_path = PathBuf::from(test_image_path);
            let img = ImageReader::open(&input_path).unwrap().decode().unwrap();
            let img_u8 = img.to_luma8();
            info!("Using test image {} instead of camera.", test_image_path);
            Some(Arc::new(tokio::sync::Mutex::new(Box::new(
                ImageCamera::new(img_u8).unwrap()))))
        },
        None => None,
    };

    let feature_level = if product_name.eq_ignore_ascii_case("Cedar-Box") {
        FeatureLevel::Diy
    } else {
        if let Some(attached_camera) = &attached_camera {
            let camera_model = attached_camera.lock().await.model();
            if camera_model == "imx296" || camera_model == "imx290" {
                FeatureLevel::Plus  // Hopper.
            } else {
                FeatureLevel::Basic  // Hopper LE.
            }
        } else {
            FeatureLevel::Diy
        }
    };

    if let Some(binning_arg) = args.binning {
        match binning_arg {
            1 | 2 | 4 => (),
            _ => {
                error!("Invalid binning argument {}, must be 1, 2, or 4",
                       binning_arg);
                std::process::exit(1);
            }
        }
    }

    let camera = get_camera(&attached_camera, &test_image_camera);
    {
        let locked_camera = camera.lock().await;
        info!("Using camera {} {}x{}",
              locked_camera.model(),
              locked_camera.dimensions().0,
              locked_camera.dimensions().1);
    }

    let shared_telescope_position = Arc::new(Mutex::new(TelescopePosition::new()));

    // Apparently when a client cancels a gRPC request (e.g. timeout), the
    // corresponding server-side tokio task is cancelled. Per
    // https://docs.rs/tokio/latest/tokio/task/index.html#cancellation
    //   "When tasks are shut down, it will stop running at whichever .await it
    //   has yielded at. All local variables are destroyed by running their
    //   destructor."
    //
    // In our code, this can have grave consequences. Consider:
    //   <acquire resource>
    //   foobar().await;
    //   <release resource>
    // Because of task cancellation, we might never regain control after the
    // .await, and thus not release the resource.
    //
    // Because tokio guarantees that locals are destroyed (I verified this),
    // RAII should be used to guard against control never coming back from
    // .await:
    //   <acquire resource in RAII object, with release on drop>
    //   foobar().await;
    //
    // Another precaution is to spawn a separate task to run long-lived or
    // transactional operations, such that if the RPC's task gets cancelled,
    // the spawned task will detach and run to completion.
    // See: https://greptime.com/blogs/2023-01-12-hidden-control-flow
    //      https://github.com/hyperium/tonic/issues/981

    // Adapted from
    // https://github.com/tokio-rs/axum/tree/main/examples/rest-grpc-multiplex
    // https://github.com/tokio-rs/axum/blob/main/examples/static-file-server

    // Build the static content web service.
    let rest = Router::new().nest_service(
        "/", ServeDir::new(flutter_app_path));

    let activity_led = Arc::new(Mutex::new(ActivityLed::new(got_signal.clone())));

    // Use supplied solver, with Tetra3Solver as fallback.
    let solver = match injected_solver {
        Some(s) => s,
        None => {
            Arc::new(tokio::sync::Mutex::new(Tetra3Solver::new(
                &args.tetra3_script, &args.tetra3_database, got_signal.clone())
                                             .await.unwrap()))
        }
    };

    // Build the gRPC service.
    let path: PathBuf = [args.log_dir, args.log_file].iter().collect();
    let cedar_server = CedarServer::new(MyCedar::new(
        solver,
        args.binning, args.display_sampling,
        invert_camera,
        /*initial_exposure_duration=*/Duration::from_millis(100),
        args.min_exposure, args.max_exposure,
        activity_led.clone(),
        attached_camera, test_image_camera, camera,
        shared_telescope_position.clone(),
        args.star_count_goal, args.sigma, args.min_sigma,
        // TODO: arg for this?
        /*stats_capacity=*/100,
        PathBuf::from(args.ui_prefs),
        path, product_name, copyright, feature_level, cedar_sky, wifi,
    ).await.unwrap());

    let grpc = tonic::transport::Server::builder()
        .accept_http1(true)  // TODO: don't need this?
        .layer(GrpcWebLayer::new())
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any))
        .add_service(cedar_server)
        .into_service();

    // Combine static content (flutter app) server and gRPC server into one service.
    let service = MultiplexService::new(rest, grpc);

    // Listen on any address for the given port.
    let addr = SocketAddr::from(([0, 0, 0, 0], 80));
    info!("Listening at {:?}", addr);
    let service_future =
        hyper::Server::bind(&addr)
        .serve(tower::make::Shared::new(service.clone()));

    let addr8080 = SocketAddr::from(([0, 0, 0, 0], 8080));
    let service_future8080 =
        hyper::Server::bind(&addr8080)
        .serve(tower::make::Shared::new(service));

    // Spin up ASCOM Alpaca server for reporting our RA/Dec solution as the
    // telescope position.

    // Function called whenever SkySafari interrogates our position.
    let async_callback = Box::new(move || {
        activity_led.lock().unwrap().received_rpc();
    });
    let alpaca_server = create_alpaca_server(shared_telescope_position, async_callback);
    let alpaca_server_future = alpaca_server.start();

    let (service_result, service_result8080, alpaca_result) =
        join!(service_future, service_future8080, alpaca_server_future);
    service_result.unwrap();
    service_result8080.unwrap();
    alpaca_result.unwrap();
}

mod multiplex_service {
    // Adapted from
    // https://github.com/tokio-rs/axum/tree/main/examples/rest-grpc-multiplex
    use axum::{
        http::Request,
        http::header::CONTENT_TYPE,
        response::{IntoResponse, Response},
    };
    use futures::{future::BoxFuture, ready};
    use std::{
        convert::Infallible,
        task::{Context, Poll},
    };
    use tower::Service;

    pub struct MultiplexService<A, B> {
        rest: A,
        rest_ready: bool,
        grpc: B,
        grpc_ready: bool,
    }

    impl<A, B> MultiplexService<A, B> {
        pub fn new(rest: A, grpc: B) -> Self {
            Self {
                rest,
                rest_ready: false,
                grpc,
                grpc_ready: false,
            }
        }
    }

    impl<A, B> Clone for MultiplexService<A, B>
    where
        A: Clone,
        B: Clone,
    {
        fn clone(&self) -> Self {
            Self {
                rest: self.rest.clone(),
                grpc: self.grpc.clone(),
                // the cloned services probably wont be ready
                rest_ready: false,
                grpc_ready: false,
            }
        }
    }

    impl<A, B> Service<Request<hyper::Body>> for MultiplexService<A, B>
    where
        A: Service<Request<hyper::Body>, Error = Infallible>,
        A::Response: IntoResponse,
        A::Future: Send + 'static,
        B: Service<Request<hyper::Body>>,
        B::Response: IntoResponse,
        B::Future: Send + 'static,
    {
        type Response = Response;
        type Error = B::Error;
        type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            // drive readiness for each inner service and record which is ready
            loop {
                match (self.rest_ready, self.grpc_ready) {
                    (true, true) => {
                        return Ok(()).into();
                    },
                    (false, _) => {
                        ready!(self.rest.poll_ready(cx)).map_err(|err| match err {})?;
                        self.rest_ready = true;
                    },
                    (_, false) => {
                        ready!(self.grpc.poll_ready(cx))?;
                        self.grpc_ready = true;
                    }
                }
            }
        }

        fn call(&mut self, req: Request<hyper::Body>) -> Self::Future {
            // require users to call `poll_ready` first, if they don't we're allowed to panic
            // as per the `tower::Service` contract
            assert!(
                self.grpc_ready,
                "grpc service not ready. Did you forget to call `poll_ready`?"
            );
            assert!(
                self.rest_ready,
                "rest service not ready. Did you forget to call `poll_ready`?"
            );

            // if we get a grpc request call the grpc service, otherwise call the rest service
            // when calling a service it becomes not-ready so we have drive readiness again
            if is_grpc_request(&req) {
                self.grpc_ready = false;
                let future = self.grpc.call(req);
                Box::pin(async move {
                    let res = future.await?;
                    Ok(res.into_response())
                })
            } else {
                self.rest_ready = false;
                let future = self.rest.call(req);
                Box::pin(async move {
                    let res = future.await.map_err(|err| match err {})?;
                    Ok(res.into_response())
                })
            }
        }
    }

    fn is_grpc_request<B>(req: &Request<B>) -> bool {
        req.headers()
            .get(CONTENT_TYPE)
            .map(|content_type| content_type.as_bytes())
            .filter(|content_type| content_type.starts_with(b"application/grpc"))
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proto_merge() {
        let mut prefs1 = Preferences{
            eyepiece_fov: Some(1.0),
            night_vision_theme: Some(true),
            ..Default::default()
        };
        let prefs2 = Preferences{
            night_vision_theme: Some(false),
            hide_app_bar: Some(true),
            ..Default::default()
        };
        let prefs2_bytes = Preferences::encode_to_vec(&prefs2);
        prefs1.merge(&*prefs2_bytes).unwrap();

        // Field present only in prefs1.
        assert_eq!(prefs1.eyepiece_fov, Some(1.0));

        // Field present on both protos, take prefs2 value.
        assert_eq!(prefs1.night_vision_theme, Some(false));

        // Field present only in prefs2.
        assert_eq!(prefs1.hide_app_bar, Some(true));
    }
}  // mod tests.
