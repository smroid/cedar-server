// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::fs;
use std::io;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use cedar_camera::abstract_camera::{AbstractCamera, Offset, bin_2x2, sample_2x2};
use cedar_camera::select_camera::{CameraInterface, select_camera};
use cedar_camera::image_camera::ImageCamera;
use canonical_error::{CanonicalError, CanonicalErrorCode};
use chrono::offset::Local;
use image::{GrayImage, ImageFormat};
use image::io::Reader as ImageReader;

use nix::time::{ClockId, clock_gettime, clock_settime};
use nix::sys::time::TimeSpec;

use clap::Parser;
use axum::Router;
use log::{debug, error, info, warn};
use prost::Message;
use tower_http::{services::ServeDir, cors::CorsLayer, cors::Any};
use tonic_web::GrpcWebLayer;

use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, registry, EnvFilter};
use tracing_appender::{non_blocking::NonBlockingBuilder};

use futures::join;

use cedar_server::astro_util::{alt_az_from_equatorial, equatorial_from_alt_az, position_angle};
use cedar_server::cedar::cedar_server::{Cedar, CedarServer};
use cedar_server::cedar::{Accuracy, ActionRequest, CalibrationData, CelestialCoordFormat,
                          EmptyMessage, FixedSettings, FrameRequest, FrameResult,
                          Image, ImageCoord, LatLong, LocationBasedInfo, MountType,
                          OperatingMode, OperationSettings, ProcessingStats, Rectangle,
                          StarCentroid, Preferences, ServerInformationRequest,
                          ServerInformationResult};
use ::cedar_server::calibrator::Calibrator;
use ::cedar_server::detect_engine::{DetectEngine, DetectResult};
use ::cedar_server::scale_image::scale_image;
use ::cedar_server::solve_engine::{PlateSolution, SolveEngine};
use ::cedar_server::position_reporter::{TelescopePosition, create_alpaca_server};
use ::cedar_server::motion_estimator::MotionEstimator;
use ::cedar_server::polar_analyzer::PolarAnalyzer;
use ::cedar_server::tetra3_subprocess::Tetra3Subprocess;
use ::cedar_server::value_stats::ValueStatsAccumulator;
use ::cedar_server::tetra3_server;
use ::cedar_server::tetra3_server::{CelestialCoord, SolveResult as SolveResultProto, SolveStatus};

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

    preferences_file: PathBuf,

    // The path to our log file.
    log_file: PathBuf,
}

struct CedarState {
    camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
    fixed_settings: Arc<Mutex<FixedSettings>>,
    calibration_data: Arc<tokio::sync::Mutex<CalibrationData>>,
    operation_settings: OperationSettings,
    detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
    tetra3_subprocess: Arc<Mutex<Tetra3Subprocess>>,
    solve_engine: Arc<tokio::sync::Mutex<SolveEngine>>,
    calibrator: Arc<tokio::sync::Mutex<Calibrator>>,
    telescope_position: Arc<Mutex<TelescopePosition>>,
    polar_analyzer: Arc<Mutex<PolarAnalyzer>>,

    // See "About Resolutions" below.
    // Whether (and how much, 2x2 or 4x4) the acquired image is binned prior to
    // CedarDetect and sending to the UI.
    binning: u32,
    // Whether (possibly binned) image is to be 2x sampled when sending to the
    // UI.
    display_sampling: bool,

    // We host the user interface preferences here. These do not affect server
    // operation; we reflect them out to all clients and persist them to a
    // server-side file.
    preferences: Preferences,

    // This is the most recent display image returned by get_frame().
    scaled_image: Option<Arc<GrayImage>>,
    scaled_image_binning_factor: u32,

    // Full resolution dimensions.
    width: u32,
    height: u32,

    calibrating: bool,
    cancel_calibration: Arc<Mutex<bool>>,
    // Relevant only if calibration is underway (`calibration_image` is present).
    calibration_start: Instant,
    calibration_duration_estimate: Duration,

    // For boresight capturing.
    center_peak_position: Arc<Mutex<Option<ImageCoord>>>,

    serve_latency_stats: ValueStatsAccumulator,
    overall_latency_stats: ValueStatsAccumulator,
}

#[tonic::async_trait]
impl Cedar for MyCedar {
    async fn get_server_information(
        &self, request: tonic::Request<ServerInformationRequest>)
        -> Result<tonic::Response<ServerInformationResult>, tonic::Status>
    {
        let req: ServerInformationRequest = request.into_inner();
        let mut response = ServerInformationResult::default();

        if let Some(log_request) = req.log_request {
            let tail = Self::read_file_tail(&self.log_file, log_request);
            if let Err(e) = tail {
                return Err(tonic::Status::failed_precondition(
                    format!("Error reading log file {:?}: {:?}.", self.log_file, e)));
            }
            response.log_content = Some(tail.unwrap());
        }

        Ok(tonic::Response::new(response))
    }

    async fn update_fixed_settings(
        &self, request: tonic::Request<FixedSettings>)
        -> Result<tonic::Response<FixedSettings>, tonic::Status>
    {
        let req: FixedSettings = request.into_inner();
        let locked_state = self.state.lock().await;
        if let Some(observer_location) = req.observer_location {
            locked_state.fixed_settings.lock().unwrap().observer_location =
                Some(observer_location.clone());
            info!("Updated observer location to {:?}", observer_location);
        }
        if let Some(current_time) = req.current_time {
            let current_time = TimeSpec::new(current_time.seconds, current_time.nanos as i64);
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
        let mut fixed_settings = locked_state.fixed_settings.lock().unwrap().clone();
        // Fill in our current time.
        Self::fill_in_time(&mut fixed_settings);
        Ok(tonic::Response::new(fixed_settings))
    }

    async fn update_operation_settings(
        &self, request: tonic::Request<OperationSettings>)
        -> Result<tonic::Response<OperationSettings>, tonic::Status> {
        let req: OperationSettings = request.into_inner();
        if let Some(new_operating_mode) = req.operating_mode {
            if new_operating_mode == OperatingMode::Setup as i32 {
                let mut locked_state = self.state.lock().await;
                if locked_state.calibrating {
                    // Cancel calibration.
                    *locked_state.cancel_calibration.lock().unwrap() = true;
                    locked_state.tetra3_subprocess.lock().unwrap().send_interrupt_signal();
                }
                if locked_state.operation_settings.operating_mode ==
                    Some(OperatingMode::Operate as i32)
                {
                    // Transition: OPERATE -> SETUP mode.

                    // In SETUP mode we run at full speed.
                    if let Err(x) = Self::set_update_interval(
                        &*locked_state, Duration::ZERO).await
                    {
                        return Err(tonic_status(x));
                    }
                    locked_state.solve_engine.lock().await.stop().await;
                    Self::reset_session_stats(locked_state.deref_mut()).await;
                    if let Err(x) = Self::set_pre_calibration_defaults(&*locked_state).await {
                        return Err(tonic_status(x));
                    }
                    locked_state.detect_engine.lock().await.set_focus_mode(
                        true, locked_state.binning);
                    locked_state.operation_settings.operating_mode =
                        Some(OperatingMode::Setup as i32);
                }
            } else if new_operating_mode == OperatingMode::Operate as i32 {
                let locked_state = self.state.lock().await;
                if locked_state.operation_settings.operating_mode ==
                    Some(OperatingMode::Setup as i32)
                {
                    // Transition: SETUP -> OPERATE mode.
                    //
                    // The SETUP -> OPERATE mode change invovles a call to
                    // calibrate() which can take several seconds. If the gRPC
                    // client aborts the RPC (e.g. due to timeout), we want the
                    // calibration and state updates (i.e. detect engine's
                    // focus_mode, our operating_mode) to be completed properly.
                    //
                    // The spawned task runs to completion even if the RPC
                    // handler task aborts.
                    //
                    // Note that below we return immediately rather than joining
                    // the task_handle. We arrange for get_frame() to return a
                    // FrameResult with a information about the ongoing
                    // calibration.
                    let state = self.state.clone();
                    let calibration_solve_timeout = Duration::from_secs(5);
                    let _task_handle: tokio::task::JoinHandle<
                            Result<tonic::Response<OperationSettings>, tonic::Status>> =
                        tokio::task::spawn(async move {
                            {
                                let mut locked_state = state.lock().await;
                                locked_state.calibrating = true;
                                locked_state.calibration_start = Instant::now();
                                locked_state.calibration_duration_estimate =
                                    Duration::from_secs(5) + calibration_solve_timeout;
                                locked_state.solve_engine.lock().await.stop().await;
                                locked_state.detect_engine.lock().await.stop().await;
                                locked_state.calibration_data.lock().await.calibration_time =
                                    Some(prost_types::Timestamp::try_from(
                                        SystemTime::now()).unwrap());
                            }
                            // No locks held.
                            let cal_result = Self::calibrate(
                                state.clone(), calibration_solve_timeout).await;
                            if let Err(x) = cal_result {
                                // The only error we expect is Aborted.
                                assert!(x.code == CanonicalErrorCode::Aborted);
                            }

                            let mut locked_state = state.lock().await;
                            locked_state.calibrating = false;
                            if *locked_state.cancel_calibration.lock().unwrap() {
                                // Calibration was cancelled. Stay in Setup mode.
                                *locked_state.cancel_calibration.lock().unwrap() = false;
                            } else {
                                // Transition into Operate mode.
                                locked_state.detect_engine.lock().await.set_focus_mode(
                                    false, locked_state.binning);
                                locked_state.solve_engine.lock().await.start().await;
                                // Restore OPERATE mode update interval.
                                let std_duration;
                                {
                                    let update_interval = locked_state.operation_settings.
                                        update_interval.clone().unwrap();
                                    std_duration = std::time::Duration::try_from(
                                        update_interval).unwrap();
                                    locked_state.operation_settings.operating_mode =
                                        Some(OperatingMode::Operate as i32);
                                }
                                if let Err(x) = Self::set_update_interval(
                                    &*locked_state, std_duration).await
                                {
                                    return Err(tonic_status(x));
                                }
                            }
                            let result = tonic::Response::new(
                                locked_state.operation_settings.clone());
                            Ok(result)
                        });
                    // Let _task_handle go out of scope, detaching the spawned
                    // calibration task to complete regardless of a possible RPC
                    // timeout.
                }
            } else {
                return Err(tonic::Status::invalid_argument(
                    format!("Got invalid operating_mode: {}.", new_operating_mode)));
            }
        }  // Update operating_mode.
        if let Some(exp_time) = req.exposure_time {
            if exp_time.seconds < 0 || exp_time.nanos < 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got negative exposure_time: {}.", exp_time)));
            }
            let std_duration = std::time::Duration::try_from(exp_time.clone()).unwrap();
            let mut locked_state = self.state.lock().await;
            if let Err(x) = Self::set_exposure_time(&*locked_state, std_duration).await {
                return Err(tonic_status(x));
            }
            locked_state.operation_settings.exposure_time = Some(exp_time);
        }
        if let Some(accuracy) = req.accuracy {
            let mut locked_state = self.state.lock().await;
            locked_state.operation_settings.accuracy = Some(accuracy);
            Self::update_accuracy_adjusted_params(&*locked_state).await;
        }
        if let Some(update_interval) = req.update_interval {
            if update_interval.seconds < 0 || update_interval.nanos < 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got negative update_interval: {}.", update_interval)));
            }
            let std_duration = std::time::Duration::try_from(
                update_interval.clone()).unwrap();
            let mut locked_state = self.state.lock().await;
            if locked_state.operation_settings.operating_mode ==
                Some(OperatingMode::Operate as i32)
            {
                if let Err(x) = Self::set_update_interval(&*locked_state,
                                                          std_duration).await {
                    return Err(tonic_status(x));
                }
            }
            locked_state.operation_settings.update_interval = Some(update_interval);
        }
        if let Some(_dwell_update_interval) = req.dwell_update_interval {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for dwell_update_interval."));
        }
        if let Some(_log_dwelled_positions) = req.log_dwelled_positions {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for log_dwelled_positions."));
        }

        Ok(tonic::Response::new(self.state.lock().await.operation_settings.clone()))
    }

    async fn update_preferences(
        &self, request: tonic::Request<Preferences>)
        -> Result<tonic::Response<Preferences>, tonic::Status> {
        let mut locked_state = self.state.lock().await;
        let req: Preferences = request.into_inner();
        if let Some(coord_format) = req.celestial_coord_format {
            locked_state.preferences.celestial_coord_format = Some(coord_format);
        }
        if let Some(eyepiece_fov) = req.eyepiece_fov {
            locked_state.preferences.eyepiece_fov = Some(eyepiece_fov);
        }
        if let Some(night_vision) = req.night_vision_theme {
            locked_state.preferences.night_vision_theme = Some(night_vision);
        }
        if let Some(show_perf) = req.show_perf_stats {
            locked_state.preferences.show_perf_stats = Some(show_perf);
        }
        if let Some(hide_app_bar) = req.hide_app_bar {
            locked_state.preferences.hide_app_bar = Some(hide_app_bar);
        }
        if let Some(mount_type) = req.mount_type {
            locked_state.preferences.mount_type = Some(mount_type);
        }

        // Write updated preferences to file.
        let prefs_path = Path::new(&self.preferences_file);
        let scratch_path = prefs_path.with_extension("tmp");

        let mut buf = vec![];
        if let Err(e) = locked_state.preferences.encode(&mut buf) {
            warn!("Could not encode preferences: {:?}", e);
            return Ok(tonic::Response::new(locked_state.preferences.clone()));
        }
        if let Err(e) = fs::write(&scratch_path, buf) {
            warn!("Could not write file: {:?}", e);
            return Ok(tonic::Response::new(locked_state.preferences.clone()));
        }
        if let Err(e) = fs::rename(scratch_path, prefs_path) {
            warn!("Could not rename file: {:?}", e);
        }

        Ok(tonic::Response::new(locked_state.preferences.clone()))
    }

    async fn get_frame(&self, request: tonic::Request<FrameRequest>)
                       -> Result<tonic::Response<FrameResult>, tonic::Status> {
        let req: FrameRequest = request.into_inner();
        let frame_result = Self::get_next_frame(
            self.state.clone(), req.prev_frame_id).await;
        Ok(tonic::Response::new(frame_result))
    }

    async fn initiate_action(&self, request: tonic::Request<ActionRequest>)
                             -> Result<tonic::Response<EmptyMessage>, tonic::Status> {
        let req: ActionRequest = request.into_inner();
        let locked_state = self.state.lock().await;
        if req.capture_boresight.unwrap_or(false) {
            let operating_mode = locked_state.operation_settings.operating_mode.or(
                    Some(OperatingMode::Setup as i32)).unwrap();
            if operating_mode == OperatingMode::Setup as i32 {
                let boresight_pos =
                    match locked_state.center_peak_position.lock().unwrap().as_ref()
                {
                    Some(pos) => Some(tetra3_server::ImageCoord{
                        x: pos.x,
                        y: pos.y,
                    }),
                    None => None,
                };
                if let Err(x) =
                    locked_state.solve_engine.lock().await.set_boresight_pixel(boresight_pos)
                {
                    return Err(tonic_status(x));
                }
            } else {
                // Operate mode.
                let plate_solution = locked_state.solve_engine.lock().await.
                    get_next_result(None).await;
                if let Some(slew_request) = plate_solution.slew_request {
                    if slew_request.target_within_center_region {
                        let boresight_pos = slew_request.image_pos.unwrap();
                        if let Err(x) = locked_state.solve_engine.lock().await.
                            set_boresight_pixel(Some(tetra3_server::ImageCoord{
                                x: boresight_pos.x,
                                y: boresight_pos.y}))
                        {
                            return Err(tonic_status(x));
                        }
                    } else {
                        return Err(tonic::Status::failed_precondition(
                            "Target not in center region."));
                    }
                } else {
                    return Err(tonic::Status::failed_precondition(
                        format!("Not in Setup mode: {:?}.", operating_mode)));
                }
            }
        }
        if req.shutdown_server.unwrap_or(false) {
            info!("Shutting down host system");
            std::thread::sleep(Duration::from_secs(2));
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
        if req.stop_slew.unwrap_or(false) {
            locked_state.telescope_position.lock().unwrap().slew_active = false;
        }
        if req.save_image.unwrap_or(false) {
            let solve_engine = &mut locked_state.solve_engine.lock().await;
            if let Err(x) = solve_engine.save_image().await {
                return Err(tonic_status(x));
            }
        }
        Ok(tonic::Response::new(EmptyMessage{}))
    }
}

impl MyCedar {
    fn fill_in_time(fixed_settings: &mut FixedSettings) {
        if let Ok(cur_time) = clock_gettime(ClockId::CLOCK_REALTIME) {
            let mut pst = prost_types::Timestamp::default();
            pst.seconds = cur_time.tv_sec();
            pst.nanos = cur_time.tv_nsec() as i32;
            fixed_settings.current_time = Some(pst);
        }
    }

    async fn set_exposure_time(state: &CedarState, exposure_time: std::time::Duration)
                               -> Result<(), CanonicalError> {
        state.detect_engine.lock().await.set_exposure_time(exposure_time).await
    }

    async fn set_update_interval(state: &CedarState, update_interval: std::time::Duration)
                                 -> Result<(), CanonicalError> {
        state.camera.lock().await.set_update_interval(update_interval)?;
        state.detect_engine.lock().await.set_update_interval(update_interval)?;
        state.solve_engine.lock().await.set_update_interval(update_interval)
    }

    async fn reset_session_stats(state: &mut CedarState) {
        state.detect_engine.lock().await.reset_session_stats();
        state.solve_engine.lock().await.reset_session_stats();
        state.serve_latency_stats.reset_session();
        state.overall_latency_stats.reset_session();
    }

    // Called when entering SETUP mode.
    async fn set_pre_calibration_defaults(state: &CedarState) -> Result<(), CanonicalError> {
        let mut locked_camera = state.camera.lock().await;
        let gain = locked_camera.optimal_gain();
        locked_camera.set_gain(gain)?;
        if let Err(e) = locked_camera.set_offset(Offset::new(3)) {
            debug!("Could not set offset: {:?}", e);
        }
        *state.calibration_data.lock().await = CalibrationData{..Default::default()};
        Ok(())
    }

    // Called when entering OPERATE mode. This always succeeds (even if
    // calibration fails), unless the callibration was cancelled in which
    // case an ABORTED error is returned.
    async fn calibrate(state: Arc<tokio::sync::Mutex<CedarState>>,
                       solve_timeout: Duration)
                       -> Result<(), CanonicalError> {
        let setup_exposure_duration;
        let binning;
        let detection_sigma;
        let star_count_goal;
        let camera;
        let calibrator;
        let cancel_calibration;
        let calibration_data;
        let detect_engine;
        let solve_engine;
        {
            let locked_state = state.lock().await;
            camera = locked_state.camera.clone();
            calibrator = locked_state.calibrator.clone();
            cancel_calibration = locked_state.cancel_calibration.clone();
            calibration_data = locked_state.calibration_data.clone();
            detect_engine = locked_state.detect_engine.clone();
            solve_engine = locked_state.solve_engine.clone();

            // What was the final exposure duration coming out of SETUP mode?
            setup_exposure_duration = camera.lock().await.get_exposure_duration();
            // For calibrations, use statically configured sigma value, not adjusted
            // by accuracy setting.
            let locked_detect_engine = detect_engine.lock().await;
            binning = locked_state.binning;
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
        _ = camera.lock().await.set_offset(offset);  // Ignore unsupported offset.
        calibration_data.lock().await.camera_offset = Some(offset.value());

        let exp_duration = match calibrator.lock().await.calibrate_exposure_duration(
            setup_exposure_duration, star_count_goal,
            binning, detection_sigma,
            cancel_calibration.clone()).await {
            Ok(ed) => ed,
            Err(e) => {
                if e.code == CanonicalErrorCode::Aborted {
                    return Err(e);
                }
                warn!{"Error while calibrating exposure duration: {:?}, using {:?}",
                      e, setup_exposure_duration};
                setup_exposure_duration  // Sane fallback value.
            }
        };
        camera.lock().await.set_exposure_duration(exp_duration)?;
        calibration_data.lock().await.target_exposure_time =
            Some(prost_types::Duration::try_from(exp_duration).unwrap());
        detect_engine.lock().await.set_calibrated_exposure_duration(exp_duration);

        match calibrator.lock().await.calibrate_optical(
            solve_engine.clone(), exp_duration, solve_timeout,
            binning, detection_sigma).await
        {
            Ok((fov, distortion, match_max_error, solve_duration)) => {
                let mut locked_calibration_data = calibration_data.lock().await;
                locked_calibration_data.fov_horizontal = Some(fov);
                locked_calibration_data.lens_distortion = Some(distortion);
                locked_calibration_data.match_max_error = Some(match_max_error);
                let sensor_width_mm = camera.lock().await.sensor_size().0;
                let lens_fl_mm =
                    sensor_width_mm / (2.0 * (fov/2.0).to_radians()).tan();
                locked_calibration_data.lens_fl_mm = Some(lens_fl_mm);
                let pixel_width_mm =
                    sensor_width_mm / camera.lock().await.dimensions().0 as f32;
                locked_calibration_data.pixel_angular_size =
                    Some((pixel_width_mm / lens_fl_mm).atan().to_degrees());

                let operation_solve_timeout =
                    std::cmp::min(
                        std::cmp::max(solve_duration * 10, Duration::from_millis(500)),
                        Duration::from_secs(1));  // TODO: max solve time cmd line arg
                let mut locked_solve_engine = solve_engine.lock().await;
                locked_solve_engine.set_fov_estimate(Some(fov))?;
                locked_solve_engine.set_distortion(distortion)?;
                locked_solve_engine.set_match_max_error(match_max_error)?;
                locked_solve_engine.set_solve_timeout(operation_solve_timeout)?;
            }
            Err(e) => {
                let mut locked_calibration_data = calibration_data.lock().await;
                locked_calibration_data.fov_horizontal = None;
                locked_calibration_data.lens_distortion = None;
                locked_calibration_data.match_max_error = None;
                let mut locked_solve_engine = solve_engine.lock().await;
                locked_solve_engine.set_fov_estimate(None)?;
                locked_solve_engine.set_distortion(0.0)?;
                locked_solve_engine.set_match_max_error(0.005)?;
                // TODO: pass this in? Should come from command line, maybe is
                // max solve time.
                locked_solve_engine.set_solve_timeout(Duration::from_secs(1))?;
                if e.code == CanonicalErrorCode::Aborted {
                    return Err(e);
                }
                warn!{"Error while calibrating optics: {:?}", e};
            }
        };
        debug!("Calibration result: {:?}", calibration_data.lock().await);
        Ok(())
    }

    async fn get_next_frame(state: Arc<tokio::sync::Mutex<CedarState>>,
                            prev_frame_id: Option<i32>)
                            -> FrameResult {
        let overall_start_time = Instant::now();

        let mut frame_result = FrameResult {..Default::default()};
        let mut fixed_settings;
        let image_rectangle;
        {
            let locked_state = state.lock().await;
            image_rectangle = Rectangle{
                origin_x: 0, origin_y: 0,
                width: locked_state.width as i32,
                height: locked_state.height as i32,
            };

            fixed_settings = locked_state.fixed_settings.lock().unwrap().clone();
            // Fill in our current time.
            Self::fill_in_time(&mut fixed_settings);
            frame_result.fixed_settings = Some(fixed_settings.clone());
            frame_result.preferences = Some(locked_state.preferences.clone());
            frame_result.operation_settings =
                Some(locked_state.operation_settings.clone());

            if locked_state.calibrating {
                frame_result.calibrating = true;
                let time_spent_calibrating = locked_state.calibration_start.elapsed();
                let mut fraction =
                    time_spent_calibrating.as_secs_f32() /
                    locked_state.calibration_duration_estimate.as_secs_f32();
                if fraction > 1.0 {
                    fraction = 1.0;
                }
                frame_result.calibration_progress = Some(fraction);

                if let Some(img) = &locked_state.scaled_image {
                    let (scaled_width, scaled_height) = img.dimensions();
                    let mut bmp_buf = Vec::<u8>::new();
                    bmp_buf.reserve((scaled_width * scaled_height) as usize);
                    img.write_to(&mut Cursor::new(&mut bmp_buf),
                                 ImageFormat::Bmp).unwrap();
                    frame_result.image = Some(Image{
                        binning_factor: locked_state.scaled_image_binning_factor as i32,
                        // Rectangle is always in full resolution coordinates.
                        rectangle: Some(image_rectangle),
                        image_data: bmp_buf,
                    });
                }
                return frame_result;
            }
        }  // locked_state.

        // Populated only in OperatingMode::Operate mode.
        let mut tetra3_solve_result: Option<SolveResultProto> = None;
        let mut plate_solution: Option<PlateSolution> = None;

        let detect_result;
        if state.lock().await.operation_settings.operating_mode.unwrap() ==
            OperatingMode::Setup as i32
        {
            detect_result = state.lock().await.detect_engine.lock().await.
                get_next_result(prev_frame_id).await;
        } else {
            plate_solution = Some(state.lock().await.solve_engine.lock().await.
                                  get_next_result(prev_frame_id).await);
            let psr = plate_solution.as_ref().unwrap();
            tetra3_solve_result = psr.tetra3_solve_result.clone();
            detect_result = psr.detect_result.clone();
        }
        let serve_start_time = Instant::now();
        let mut locked_state = state.lock().await;

        frame_result.frame_id = detect_result.frame_id;
        let captured_image = &detect_result.captured_image;
        frame_result.exposure_time = Some(prost_types::Duration::try_from(
            captured_image.capture_params.exposure_duration).unwrap());
        frame_result.capture_time = Some(prost_types::Timestamp::try_from(
            captured_image.readout_time).unwrap());
        frame_result.camera_temperature_celsius = captured_image.temperature.0 as f32;

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

        let display_sampling = locked_state.display_sampling;

        let peak_value;
        if let Some(fa) = &detect_result.focus_aid {
            peak_value = fa.center_peak_value;
            frame_result.center_region = Some(Rectangle {
                origin_x: detect_result.center_region.left(),
                origin_y: detect_result.center_region.top(),
                width: detect_result.center_region.width() as i32,
                height: detect_result.center_region.height() as i32});

            let ic = ImageCoord {
                x: fa.center_peak_position.0 as f32,
                y: fa.center_peak_position.1 as f32,
            };
            *locked_state.center_peak_position.lock().unwrap() = Some(ic.clone());
            frame_result.center_peak_position = Some(ic);
            frame_result.center_peak_value = Some(fa.center_peak_value as i32);

            // Populate `center_peak_image`.
            let center_peak_image = &fa.peak_image;
            let peak_image_region = &fa.peak_image_region;
            let (center_peak_width, center_peak_height) =
                center_peak_image.dimensions();
            let mut center_peak_bmp_buf = Vec::<u8>::new();
            // center_peak_image_image is taken from the camera's full
            // resolution acquired image. If it is a color camera, we 2x2 bin it
            // to avoid displaying the Bayer grid.
            let binning_factor;
            if locked_state.camera.lock().await.is_color() {
                let binned_center_peak_image = bin_2x2(center_peak_image.clone());
                binning_factor = 2;
                center_peak_bmp_buf.reserve(
                    (center_peak_width / 2 * center_peak_height / 2) as usize);
                binned_center_peak_image.write_to(&mut Cursor::new(&mut center_peak_bmp_buf),
                                                  ImageFormat::Bmp).unwrap();
            } else {
                binning_factor = 1;
                center_peak_bmp_buf.reserve(
                    (center_peak_width * center_peak_height) as usize);
                center_peak_image.write_to(&mut Cursor::new(&mut center_peak_bmp_buf),
                                           ImageFormat::Bmp).unwrap();
            }
            frame_result.center_peak_image = Some(Image{
                binning_factor,
                rectangle: Some(Rectangle{
                    origin_x: peak_image_region.left(),
                    origin_y: peak_image_region.top(),
                    width: peak_image_region.width() as i32,
                    height: peak_image_region.height() as i32,
                }),
                image_data: center_peak_bmp_buf,
            });
        } else {
            peak_value = detect_result.peak_star_pixel;
            *locked_state.center_peak_position.lock().unwrap() = None;
        }

        // Populate `image` as requested.
        let mut disp_image = &captured_image.image;
        if detect_result.binned_image.is_some() {
            disp_image = detect_result.binned_image.as_ref().unwrap();
        }
        let mut resized_disp_image = disp_image;
        let resize_result: Arc<GrayImage>;
        if display_sampling {
            resize_result = Arc::new(sample_2x2(disp_image.deref().clone()));
            resized_disp_image = &resize_result;
        }

        let mut bmp_buf = Vec::<u8>::new();
        let (width, height) = resized_disp_image.dimensions();
        bmp_buf.reserve((width * height) as usize);
        let scaled_image = scale_image(resized_disp_image,
                                       detect_result.display_black_level,
                                       peak_value,
                                       /*gamma=*/0.7);
        // Save most recent display image.
        locked_state.scaled_image = Some(Arc::new(scaled_image.clone()));
        scaled_image.write_to(&mut Cursor::new(&mut bmp_buf),
                              ImageFormat::Bmp).unwrap();

        let binning_factor = locked_state.binning * if display_sampling { 2 } else { 1 };
        locked_state.scaled_image_binning_factor = binning_factor;
        frame_result.image = Some(Image{
            binning_factor: binning_factor as i32,
            // Rectangle is always in full resolution coordinates.
            rectangle: Some(image_rectangle),
            image_data: bmp_buf,
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
        if plate_solution.is_some() {
            let psr = &plate_solution.as_ref().unwrap();
            stats.solve_interval = Some(psr.solve_interval_stats.clone());
            stats.solve_latency = Some(psr.solve_latency_stats.clone());
            stats.solve_attempt_fraction =
                Some(psr.solve_attempt_stats.clone());
            stats.solve_success_fraction =
                Some(psr.solve_success_stats.clone());
            frame_result.slew_request = psr.slew_request.clone();
            if let Some(boresight_image) = &psr.boresight_image {
                let mut bmp_buf = Vec::<u8>::new();
                let bsi_rect = psr.boresight_image_region.unwrap();
                // boresight_image is taken from the camera's acquired image. In
                // OPERATE mode the camera capture is always full resolution. If
                // it is a color camera, we 2x2 bin it to avoid displaying the
                // Bayer grid.
                let binning_factor;
                if locked_state.camera.lock().await.is_color() {
                    let binned_boresight_image = bin_2x2(boresight_image.clone());
                    binning_factor = 2;
                    bmp_buf.reserve((bsi_rect.width() / 2 * bsi_rect.height() / 2) as usize);
                    binned_boresight_image.write_to(&mut Cursor::new(&mut bmp_buf),
                                                    ImageFormat::Bmp).unwrap();
                } else {
                    binning_factor = 1;
                    bmp_buf.reserve((bsi_rect.width() * bsi_rect.height()) as usize);
                    boresight_image.write_to(&mut Cursor::new(&mut bmp_buf),
                                             ImageFormat::Bmp).unwrap();
                }
                frame_result.boresight_image = Some(Image{
                    binning_factor,
                    // Rectangle is always in full resolution coordinates.
                    rectangle: Some(Rectangle{origin_x: bsi_rect.left(),
                                              origin_y: bsi_rect.top(),
                                              width: bsi_rect.width() as i32,
                                              height: bsi_rect.height() as i32}),
                    image_data: bmp_buf,
                });
            }
        }
        if tetra3_solve_result.is_some() {
            let tsr = &tetra3_solve_result.unwrap();
            frame_result.plate_solution = Some(tsr.clone());
            if tsr.status == Some(SolveStatus::MatchFound.into()) {
                let celestial_coords;
                if tsr.target_coords.len() > 0 {
                    celestial_coords = tsr.target_coords[0].clone();
                } else {
                    celestial_coords = tsr.image_center_coords.as_ref().unwrap().clone();
                }
                let bs_ra = celestial_coords.ra.to_radians() as f64;
                let bs_dec = celestial_coords.dec.to_radians() as f64;

                if frame_result.slew_request.is_some() &&
                    locked_state.preferences.mount_type == Some(MountType::Equatorial.into())
                {
                    let slew_request = frame_result.slew_request.as_mut().unwrap();
                    // Compute the movement required in RA and Dec to move boresight to
                    // target.
                    let target_ra = slew_request.target.as_ref().unwrap().ra;
                    let mut rel_ra = target_ra - bs_ra.to_degrees() as f32;
                    if rel_ra < -180.0 {
                        rel_ra += 360.0;
                    }
                    if rel_ra > 180.0 {
                        rel_ra -= 360.0;
                    }
                    slew_request.offset_rotation_axis = Some(rel_ra);

                    let target_dec = slew_request.target.as_ref().unwrap().dec;
                    let rel_dec = target_dec - bs_dec.to_degrees() as f32;
                    slew_request.offset_tilt_axis = Some(rel_dec);
                }
                if fixed_settings.observer_location.is_some() {
                    let geo_location = fixed_settings.observer_location.clone().unwrap();
                    let lat = geo_location.latitude.to_radians() as f64;
                    let long = geo_location.longitude.to_radians() as f64;
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
                        bs_ra, bs_dec, z_ra, z_dec).to_degrees() as f32 +
                                                 tsr.roll.unwrap()) % 360.0;
                    // Arrange for angle to be 0..360.
                    if zenith_roll_angle < 0.0 {
                        zenith_roll_angle += 360.0;
                    }
                    frame_result.location_based_info =
                        Some(LocationBasedInfo{zenith_roll_angle,
                                               altitude: bs_alt.to_degrees() as f32,
                                               azimuth: bs_az.to_degrees() as f32,
                                               hour_angle: bs_ha.to_degrees() as f32,
                        });

                    if frame_result.slew_request.is_some() &&
                        locked_state.preferences.mount_type == Some(MountType::AltAz.into())
                    {
                        let slew_request = frame_result.slew_request.as_mut().unwrap();
                        // Compute the movement required in azimuith and altitude to move
                        // boresight to target.
                        let target_ra = slew_request.target.as_ref().unwrap().ra;
                        let target_dec = slew_request.target.as_ref().unwrap().dec;
                        let (target_alt, target_az, _target_ha) =
                            alt_az_from_equatorial(target_ra.to_radians() as f64,
                                                   target_dec.to_radians() as f64,
                                                   lat, long, time);
                        let mut rel_az = target_az.to_degrees() - bs_az.to_degrees();
                        if rel_az < -180.0 {
                            rel_az += 360.0;
                        }
                        if rel_az > 180.0 {
                            rel_az -= 360.0;
                        }
                        slew_request.offset_rotation_axis = Some(rel_az as f32);

                        let rel_alt = target_alt.to_degrees() - bs_alt.to_degrees();
                        slew_request.offset_tilt_axis = Some(rel_alt as f32);
                    }
                }
            }
        }
        let boresight_position =
            locked_state.solve_engine.lock().await.boresight_pixel().expect(
                "solve_engine.boresight_pixel() should not fail");
        if let Some(bs) = boresight_position {
            frame_result.boresight_position = Some(ImageCoord{x: bs.x, y: bs.y});
        } else {
            frame_result.boresight_position =
                Some(ImageCoord{x: locked_state.width as f32 / 2.0,
                                y: locked_state.height as f32 / 2.0});
        }
        frame_result.calibration_data =
            Some(locked_state.calibration_data.lock().await.clone());
        frame_result.polar_align_advice = Some(
            locked_state.polar_analyzer.lock().unwrap().get_polar_align_advice());

        frame_result
    }

    pub async fn new(min_exposure_duration: Duration,
                     max_exposure_duration: Duration,
                     tetra3_script: String,
                     tetra3_database: String,
                     tetra3_uds: String,
                     camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
                     telescope_position: Arc<Mutex<TelescopePosition>>,
                     binning: u32,
                     display_sampling: bool,
                     base_star_count_goal: i32,
                     base_detection_sigma: f32,
                     min_detection_sigma: f32,
                     stats_capacity: usize,
                     preferences_file: PathBuf,
                     log_file: PathBuf) -> Self {
        let detect_engine = Arc::new(tokio::sync::Mutex::new(DetectEngine::new(
            min_exposure_duration, max_exposure_duration,
            min_detection_sigma, base_detection_sigma,
            base_star_count_goal,
            camera.clone(),
            /*update_interval=*/Duration::ZERO,
            /*auto_exposure=*/true,
            /*focus_mode_enabled=*/true,
            stats_capacity)));
        let tetra3_subprocess = Arc::new(Mutex::new(
            Tetra3Subprocess::new(tetra3_script, tetra3_database).unwrap()));
        let mut preferences = Preferences{
            celestial_coord_format: Some(CelestialCoordFormat::HmsDms.into()),
            eyepiece_fov: Some(1.0),
            night_vision_theme: Some(false),
            show_perf_stats: Some(false),
            hide_app_bar: Some(false),
            mount_type: Some(MountType::Equatorial.into()),
        };

        // Load UI preferences file.
        let prefs_path = Path::new(&preferences_file);
        let bytes = fs::read(prefs_path);
        if let Err(e) = bytes {
            warn!("Could not read file {:?}: {:?}", preferences_file, e);
        } else {
            match Preferences::decode(bytes.unwrap().as_slice()) {
                Ok(mut p) => {
                    if p.eyepiece_fov.unwrap() < 0.1 {
                        p.eyepiece_fov = Some(0.1);
                    }
                    if p.eyepiece_fov.unwrap() > 2.0 {
                        p.eyepiece_fov = Some(2.0);
                    }
                    preferences = p;
                }
                Err(e) => {
                    warn!("Could not decode preferences {:?}", e);
                },
            }
        }

        let fixed_settings = Arc::new(Mutex::new(FixedSettings {
            observer_location: None,
            current_time: None,
            session_name: None,
            max_exposure_time: Some(
                prost_types::Duration::try_from(max_exposure_duration).unwrap()),
        }));

        let polar_analyzer = Arc::new(Mutex::new(PolarAnalyzer::new()));

        // Define callback invoked from SolveEngine().
        let closure_fixed_settings = fixed_settings.clone();
        let closure_telescope_position = telescope_position.clone();
        let motion_estimator = Arc::new(Mutex::new(MotionEstimator::new(
            /*gap_tolerance=*/Duration::from_secs(3),
            /*bump_tolerance=*/Duration::from_secs_f32(2.0))));
        let closure_polar_analyzer = polar_analyzer.clone();
        let closure = Arc::new(move |detect_result: Option<DetectResult>,
                                     solve_result_proto: Option<SolveResultProto>|
        {
            Self::solution_callback(
                detect_result,
                solve_result_proto,
                closure_fixed_settings.lock().unwrap().observer_location.clone(),
                &mut closure_telescope_position.lock().unwrap(),
                &mut motion_estimator.lock().unwrap(),
                &mut closure_polar_analyzer.lock().unwrap())
        });
        let dimensions = camera.lock().await.dimensions();
        let state = Arc::new(tokio::sync::Mutex::new(CedarState {
            camera: camera.clone(),
            fixed_settings,
            operation_settings: OperationSettings {
                operating_mode: Some(OperatingMode::Setup as i32),
                exposure_time: Some(prost_types::Duration {
                    seconds: 0, nanos: 0,
                }),
                accuracy: Some(Accuracy::Balanced.into()),
                update_interval: Some(prost_types::Duration {
                    seconds: 0, nanos: 0,
                }),
                dwell_update_interval: Some(prost_types::Duration {
                    seconds: 1, nanos: 0,
                }),
                log_dwelled_positions: Some(false),
            },
            calibration_data: Arc::new(tokio::sync::Mutex::new(
                CalibrationData{..Default::default()})),
            detect_engine: detect_engine.clone(),
            tetra3_subprocess: tetra3_subprocess.clone(),
            solve_engine: Arc::new(tokio::sync::Mutex::new(SolveEngine::new(
                tetra3_subprocess.clone(), detect_engine.clone(), tetra3_uds,
                /*update_interval=*/Duration::ZERO,
                stats_capacity, closure).await.unwrap())),
            calibrator: Arc::new(tokio::sync::Mutex::new(
                Calibrator::new(camera.clone()))),
            telescope_position,
            polar_analyzer,
            binning, display_sampling,
            preferences,
            scaled_image: None,
            scaled_image_binning_factor: 1,
            width: dimensions.0 as u32,
            height: dimensions.1 as u32,
            calibrating: false,
            cancel_calibration: Arc::new(Mutex::new(false)),
            calibration_start: Instant::now(),
            calibration_duration_estimate: Duration::MAX,
            center_peak_position: Arc::new(Mutex::new(None)),
            serve_latency_stats: ValueStatsAccumulator::new(stats_capacity),
            overall_latency_stats: ValueStatsAccumulator::new(stats_capacity),
        }));
        let cedar = MyCedar {
            state: state.clone(),
            preferences_file,
            log_file,
        };
        // Set pre-calibration defaults on camera.
        let locked_state = state.lock().await;
        if let Err(x) = Self::set_pre_calibration_defaults(&*locked_state).await {
            warn!("Could not set default settings on camera {:?}", x);
        }
        locked_state.detect_engine.lock().await.set_focus_mode(true, binning);
        Self::update_accuracy_adjusted_params(&*locked_state).await;

        cedar
    }

    async fn update_accuracy_adjusted_params(state: &CedarState) {
        let accuracy = state.operation_settings.accuracy.unwrap();
        let acc_enum = Accuracy::try_from(accuracy).unwrap();
        let multiplier = match acc_enum {
            Accuracy::Faster => 0.7,
            Accuracy::Balanced => 1.0,
            Accuracy::Accurate => 1.4,
            _ => 1.0,
        };
        let mut locked_detect_engine = state.detect_engine.lock().await;
        locked_detect_engine.set_accuracy_multiplier(multiplier);
    }

    fn read_file_tail(log_file: &PathBuf, bytes_to_read: i32) -> io::Result<String> {
        let mut f = fs::File::open(log_file)?;
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

    fn solution_callback(detect_result: Option<DetectResult>,
                         solve_result_proto: Option<SolveResultProto>,
                         geo_location: Option<LatLong>,
                         telescope_position: &mut TelescopePosition,
                         motion_estimator: &mut MotionEstimator,
                         polar_analyzer: &mut PolarAnalyzer) -> Option<CelestialCoord> {
        if solve_result_proto.is_none() {
            telescope_position.boresight_valid = false;
            if let Some(detect_result) = detect_result {
                motion_estimator.add(detect_result.captured_image.readout_time, None, None);
            }
        } else {
            let solve_result_proto = solve_result_proto.unwrap();
            // Update SkySafari telescope interface with our position.
            let coords;
            if solve_result_proto.target_coords.len() > 0 {
                coords = solve_result_proto.target_coords[0].clone();
            } else {
                coords = solve_result_proto.image_center_coords.as_ref().unwrap().clone();
            }
            telescope_position.boresight_ra = coords.ra as f64;
            telescope_position.boresight_dec = coords.dec as f64;
            telescope_position.boresight_valid = true;
            let readout_time = detect_result.unwrap().captured_image.readout_time;
            motion_estimator.add(readout_time, Some(coords.clone()), solve_result_proto.rmse);
            if let Some(geo_location) = geo_location {
                let lat = geo_location.latitude.to_radians() as f64;
                let long = geo_location.longitude.to_radians() as f64;
                let bs_ra = coords.ra.to_radians() as f64;
                let bs_dec = coords.dec.to_radians() as f64;
                // alt/az of boresight. Also boresight hour angle.
                let (_alt, _az, ha) =
                    alt_az_from_equatorial(bs_ra, bs_dec, lat, long, readout_time);
                polar_analyzer.process_solution(&coords,
                                                ha.to_degrees() as f32,
                                                geo_location.latitude,
                                                &motion_estimator.get_estimate());
            }
        }
        if telescope_position.slew_active {
            Some(CelestialCoord{ra: telescope_position.slew_target_ra as f32,
                                dec: telescope_position.slew_target_dec as f32})
        } else {
            None
        }
    }
}

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

#[derive(Parser, Debug)]
#[command(author, version, about, long_about=None)]
struct Args {
    /// Path to tetra3_server.py script. Either set this on command line or set
    /// up a symlink. Note that PYPATH must be set to include the tetra3.py
    /// library location.
    #[arg(long, default_value = "./tetra3_server.py")]
    tetra3_script: String,

    /// Star catalog database for Tetra3 to load.
    #[arg(long, default_value = "default_database")]
    tetra3_database: String,

    /// Unix domain socket file for Tetra3 gRPC server. Server creates this file.
    #[arg(long, default_value = "/tmp/cedar.sock")]
    tetra3_socket: String,

    /// Camera interface to look for. Useful if there are multiple interfaces.
    /// Currently supported values are "asi" and "rpi". Leave empty if you
    /// are using only one camera interface, which is typical.
    #[arg(long, default_value = "")]
    camera_interface: String,

    /// Which camera (within the chosen camera interface) to use. Leave at 0
    /// if you only have one camera on the chosen interface, which is typical.
    #[arg(long, default_value_t = 0)]
    camera_index: i32,

    /// Specifies whether binning is applied prior to CedarDetect processing,
    /// and if so whether it is 2x2 binning or 4x4 binning. Legal values are 1
    /// (no binning), 2, or 4.
    /// Omit this to use the resolution-determined value.
    #[arg(long)]
    binning: Option<u32>,

    /// Specifies whether 2x2 sampling is applied (in addition to binning, if
    /// any) when sending mode images to the UI.
    /// Omit this to use the resolution-determined value.
    #[arg(long)]
    display_sampling: Option<bool>,

    /// Test image to use instead of camera.
    #[arg(long, default_value = "")]
    test_image: String,

    /// Minimum exposure duration, seconds.
    #[arg(long, value_parser = parse_duration, default_value = "0.00001")]
    min_exposure: Duration,

    /// Maximum exposure duration, seconds.
    // For monochrome camera and f/1.4 lens, 200ms is a good maximum. For color
    // camera and/or slower f/number, increase the maximum exposure accordingly.
    #[arg(long, value_parser = parse_duration, default_value = "1.0")]
    max_exposure: Duration,

    /// Target number of detected stars for auto-exposure. This is altered by
    /// the OperationSettings.accuracy setting (multiplier ranging from 0.7 to
    /// 1.4).
    #[arg(long, default_value_t = 20)]
    star_count_goal: i32,

    /// The S/N factor used to determine if a background-subtracted pixel is
    /// bright enough relative to the noise measure to be considered part of a
    /// star. This is altered by the OperationSettings.accuracy setting
    /// (multiplier ranging from 0.7 to 1.4).
    #[arg(long, default_value_t = 8.0)]
    sigma: f32,

    /// Specifies a value below which `sigma` is not adjusted by the
    /// OperationSettings.accuracy setting.
    #[arg(long, default_value_t = 5.0)]
    min_sigma: f32,

    /// Path to UI preferences file.
    #[arg(long, default_value = "./cedar_ui_prefs.binpb")]
    ui_prefs: String,

    /// Directory for log file(s).
    #[arg(long, default_value = ".")]
    log_dir: String,

    /// Name of log file.
    #[arg(long, default_value = "cedar_log.txt")]
    log_file: String,

    // TODO: max solve time
}

// Adapted from
// https://stackoverflow.com/questions/72313616/using-claps-deriveparser-how-can-i-accept-a-stdtimeduration
fn parse_duration(arg: &str)
                  -> Result<std::time::Duration, std::num::ParseFloatError> {
    let seconds = arg.parse()?;
    Ok(std::time::Duration::from_secs_f32(seconds))
}

// Adapted from
// https://github.com/tokio-rs/axum/tree/main/examples/rest-grpc-multiplex
// https://github.com/tokio-rs/axum/blob/main/examples/static-file-server
#[tokio::main]
async fn main() {
    let args = Args::parse();

    let file_appender = tracing_appender::rolling::never(&args.log_dir, &args.log_file);
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

    info!("Using Tetra3 server {:?} listening at {:?}",
          args.tetra3_script, args.tetra3_socket);
    // Build the static content web service.
    let rest = Router::new().nest_service(
        "/", ServeDir::new("../cedar_flutter/build/web"));

    let camera_interface = match args.camera_interface.as_str() {
        "" => None,
        "asi" => Some(CameraInterface::ASI),
        "rpi" => Some(CameraInterface::Rpi),
        _ => {
            error!("Unrecognized 'camera_interface' value: {}", args.camera_interface);
            std::process::exit(1);
        }
    };
    let abstract_cam = match select_camera(camera_interface, args.camera_index) {
        Ok(cam) => cam,
        Err(e) => {
            error!("Could not select camera: {:?}", e);
            std::process::exit(1);
        }
    };
    info!("Using camera {} {}x{}",
          abstract_cam.model(),
          abstract_cam.dimensions().0,
          abstract_cam.dimensions().1);
    let mpix = (abstract_cam.dimensions().0 * abstract_cam.dimensions().1) as f64 / 1000000.0;

    let camera: Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>> =
        match args.test_image.as_str() {
        "" => Arc::new(tokio::sync::Mutex::new(abstract_cam)),
        _ => {
            let input_path = PathBuf::from(&args.test_image);
            let img = ImageReader::open(&input_path).unwrap().decode().unwrap();
            let img_u8 = img.to_luma8();
            info!("Using test image {} instead of camera.", args.test_image);
            Arc::new(tokio::sync::Mutex::new(Box::new(ImageCamera::new(img_u8).unwrap())))
        },
    };

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
    if let Some(binning_arg) = args.binning {
        match binning_arg {
            1 | 2 | 4 => (),
            _ => {
                error!("Invalid binning argument {}, must be 1, 2, or 4",
                       binning_arg);
                std::process::exit(1);
            }
        }
        binning = binning_arg;
    }
    if let Some(display_sampling_arg) = args.display_sampling {
        display_sampling = display_sampling_arg;
    }
    debug!("For {:.1}mpix, binning {}, display_sampling {}",
           mpix, binning, display_sampling);

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

    // Build the gRPC service.
    let path: PathBuf = [args.log_dir, args.log_file].iter().collect();
    let grpc = tonic::transport::Server::builder()
        .accept_http1(true)
        .layer(GrpcWebLayer::new())
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any))
        .add_service(CedarServer::new(MyCedar::new(
            args.min_exposure, args.max_exposure,
            args.tetra3_script, args.tetra3_database, args.tetra3_socket,
            camera, shared_telescope_position.clone(),
            binning, display_sampling,
            args.star_count_goal, args.sigma, args.min_sigma,
            // TODO: arg for this?
            /*stats_capacity=*/100,
            PathBuf::from(args.ui_prefs),
            path,
        ).await
        )).into_service();

    // Combine static content (flutter app) server and gRPC server into one service.
    let service = MultiplexService::new(rest, grpc);

    // Listen on any address for the given port.
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    info!("Listening at {:?}", addr);

    let service_future =
        hyper::Server::bind(&addr).serve(tower::make::Shared::new(service));

    // Spin up ASCOM Alpaca server for reporting our RA/Dec solution as the
    // telescope position.
    let alpaca_server = create_alpaca_server(shared_telescope_position);
    let alpaca_server_future = alpaca_server.start();

    let (service_result, alpaca_result) = join!(service_future, alpaca_server_future);
    service_result.unwrap();
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
                    }
                    (false, _) => {
                        ready!(self.rest.poll_ready(cx)).map_err(|err| match err {})?;
                        self.rest_ready = true;
                    }
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
