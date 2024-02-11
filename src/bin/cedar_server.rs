use std::io::Cursor;
use std::net::SocketAddr;
use std::ops::DerefMut;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use camera_service::abstract_camera::{AbstractCamera, Offset};
use camera_service::asi_camera;
use camera_service::image_camera::ImageCamera;
use canonical_error::{CanonicalError, CanonicalErrorCode};
use image::{GrayImage, ImageOutputFormat};
use image::io::Reader as ImageReader;

use clap::Parser;
use axum::Router;
use log::{info, warn};
use tower_http::{services::ServeDir, cors::CorsLayer, cors::Any};
use tonic_web::GrpcWebLayer;
use tracing_subscriber;

use futures::join;

use cedar::cedar::cedar_server::{Cedar, CedarServer};
use cedar::cedar::{Accuracy, ActionRequest, CalibrationData,
                   EmptyMessage, FixedSettings, FrameRequest, FrameResult,
                   Image, ImageCoord, ImageMode, OperatingMode, OperationSettings,
                   ProcessingStats, Rectangle, StarCentroid};
use ::cedar::calibrator::Calibrator;
use ::cedar::detect_engine::DetectEngine;
use ::cedar::scale_image::scale_image;
use ::cedar::solve_engine::{PlateSolution, SolveEngine};
use ::cedar::position_reporter::{TelescopePosition, create_alpaca_server};
use ::cedar::tetra3_subprocess::Tetra3Subprocess;
use ::cedar::value_stats::ValueStatsAccumulator;
use ::cedar::tetra3_server;
use ::cedar::tetra3_server::SolveResult as SolveResultProto;

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
    state: Arc<tokio::sync::Mutex<CedarState>>,
}

struct CedarState {
    camera: Arc<tokio::sync::Mutex<dyn AbstractCamera + Send>>,
    fixed_settings: Mutex<FixedSettings>,
    calibration_data: Arc<tokio::sync::Mutex<CalibrationData>>,
    operation_settings: Mutex<OperationSettings>,
    detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
    tetra3_subprocess: Arc<Mutex<Tetra3Subprocess>>,
    solve_engine: Arc<tokio::sync::Mutex<SolveEngine>>,
    calibrator: Arc<tokio::sync::Mutex<Calibrator>>,

    // This is the most recent display image returned by get_frame().
    scaled_image: Option<Arc<GrayImage>>,
    // Full resolution dimensions.
    width: u32,
    height: u32,

    calibrating: bool,
    cancel_calibration: Arc<Mutex<bool>>,
    // Relevant only if calibration is underway (`calibration_image` is present).
    calibration_start: Instant,
    calibration_duration_estimate: Duration,

    base_star_count_goal: i32,
    base_detection_sigma: f32,
    min_detection_sigma: f32,

    // For boresight capturing.
    center_peak_position: Arc<Mutex<Option<ImageCoord>>>,

    serve_latency_stats: ValueStatsAccumulator,
    overall_latency_stats: ValueStatsAccumulator,
}

#[tonic::async_trait]
impl Cedar for MyCedar {
    // TODO: get_server_information RPC.

    async fn update_fixed_settings(
        &self, request: tonic::Request<FixedSettings>)
        -> Result<tonic::Response<FixedSettings>, tonic::Status>
    {
        let req: FixedSettings = request.into_inner();
        if req.observer_location.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateFixedSettings not implemented for observer_location."));
        }
        if req.client_time.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateFixedSettings not implemented for client_time."));
        }
        if req.session_name.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateFixedSettings not implemented for session_name."));
        }
        Ok(tonic::Response::new(
            self.state.lock().await.fixed_settings.lock().unwrap().clone()))
    }

    async fn update_operation_settings(
        &self, request: tonic::Request<OperationSettings>)
        -> Result<tonic::Response<OperationSettings>, tonic::Status> {
        let req: OperationSettings = request.into_inner();
        if req.operating_mode.is_some() {
            let new_operating_mode = req.operating_mode.unwrap();
            if new_operating_mode == OperatingMode::Setup as i32 {
                let mut locked_state = self.state.lock().await;
                if locked_state.calibrating {
                    // Cancel calibration.
                    *locked_state.cancel_calibration.lock().unwrap() = true;
                    locked_state.tetra3_subprocess.lock().unwrap().send_interrupt_signal();
                }
                if locked_state.operation_settings.lock().unwrap().operating_mode ==
                    Some(OperatingMode::Operate as i32)
                {
                    // Transition: OPERATE -> SETUP mode.
                    locked_state.solve_engine.lock().await.stop().await;
                    locked_state.detect_engine.lock().await.set_focus_mode(true);
                    Self::reset_session_stats(locked_state.deref_mut()).await;
                    if let Err(x) = Self::set_pre_calibration_defaults(&*locked_state).await {
                        return Err(tonic_status(x));
                    }
                    locked_state.operation_settings.lock().unwrap().operating_mode =
                        Some(OperatingMode::Setup as i32);
                }
            } else if new_operating_mode == OperatingMode::Operate as i32 {
                let locked_state = self.state.lock().await;
                if locked_state.operation_settings.lock().unwrap().operating_mode ==
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
                    let solve_timeout = Duration::from_secs(5);
                    let _task_handle: tokio::task::JoinHandle<
                            Result<tonic::Response<OperationSettings>, tonic::Status>> =
                        tokio::task::spawn(async move {
                            {
                                let mut locked_state = state.lock().await;
                                locked_state.calibrating = true;
                                locked_state.calibration_start = Instant::now();
                                locked_state.calibration_duration_estimate =
                                    Duration::from_secs(2) + solve_timeout;
                                locked_state.solve_engine.lock().await.stop().await;
                                locked_state.detect_engine.lock().await.stop().await;
                                locked_state.calibration_data.lock().await.calibration_time =
                                    Some(prost_types::Timestamp::try_from(
                                        SystemTime::now()).unwrap());
                            }
                            // No locks held.
                            let cal_result = Self::calibrate(state.clone(), solve_timeout).await;
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
                                locked_state.detect_engine.lock().await.set_focus_mode(false);
                                locked_state.solve_engine.lock().await.start().await;
                                locked_state.operation_settings.lock().unwrap().operating_mode =
                                    Some(OperatingMode::Operate as i32);
                            }
                            let result = tonic::Response::new(
                                locked_state.operation_settings.lock().unwrap().clone());
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
        if req.exposure_time.is_some() {
            let exp_time = req.exposure_time.unwrap();
            if exp_time.seconds < 0 || exp_time.nanos < 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got negative exposure_time: {}.", exp_time)));
            }
            let std_duration = std::time::Duration::try_from(exp_time.clone()).unwrap();
            let locked_state = self.state.lock().await;
            match Self::set_exposure_time(&*locked_state, std_duration).await {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            locked_state.operation_settings.lock().unwrap().exposure_time =
                Some(exp_time);
        }
        if req.accuracy.is_some() {
            let accuracy = req.accuracy.unwrap();
            let locked_state = self.state.lock().await;
            locked_state.operation_settings.lock().unwrap().accuracy = Some(accuracy);
            Self::update_accuracy_adjusted_params(&*locked_state).await;
        }
        if req.detection_max_size.is_some() {
            let max_size = req.detection_max_size.unwrap();
            if max_size <= 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got non-positive detection_max_size: {}.", max_size)));
            }
            let locked_state = self.state.lock().await;
            match Self::set_detection_max_size(&*locked_state, max_size).await {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            locked_state.operation_settings.lock().unwrap().detection_max_size =
                Some(max_size);
        }
        if req.update_interval.is_some() {
            let update_interval = req.update_interval.unwrap();
            if update_interval.seconds < 0 || update_interval.nanos < 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got negative update_interval: {}.", update_interval)));
            }
            let std_duration = std::time::Duration::try_from(
                update_interval.clone()).unwrap();
            let locked_state = self.state.lock().await;
            match Self::set_update_interval(&*locked_state, std_duration).await {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            locked_state.operation_settings.lock().unwrap().update_interval =
                Some(update_interval);
        }
        if req.dwell_update_interval.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for dwell_update_interval."));
        }
        if req.log_dwelled_positions.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for log_dwelled_positions."));
        }

        Ok(tonic::Response::new(
            self.state.lock().await.operation_settings.lock().unwrap().clone()))
   }

    async fn get_frame(&self, request: tonic::Request<FrameRequest>)
                       -> Result<tonic::Response<FrameResult>, tonic::Status> {
        let req: FrameRequest = request.into_inner();
        let frame_result = Self::get_next_frame(
            self.state.clone(), req.prev_frame_id, req.main_image_mode).await;
        Ok(tonic::Response::new(frame_result))
    }

    async fn initiate_action(&self, request: tonic::Request<ActionRequest>)
                             -> Result<tonic::Response<EmptyMessage>, tonic::Status> {
        let req: ActionRequest = request.into_inner();
        let locked_state = self.state.lock().await;
        if req.capture_boresight.unwrap_or(false) {
            let operating_mode =
                locked_state.operation_settings.lock().unwrap().operating_mode.or(
                    Some(OperatingMode::Setup as i32)).unwrap();
            if operating_mode != OperatingMode::Setup as i32 {
                return Err(tonic::Status::failed_precondition(
                    format!("Not in Setup mode: {:?}.", operating_mode)));
            }
            let solve_engine = &mut locked_state.solve_engine.lock().await;
            let cpp = locked_state.center_peak_position.lock().unwrap();
            match solve_engine.set_target_pixel(
                match cpp.as_ref() {
                    Some(pos) => Some(tetra3_server::ImageCoord{
                        x: pos.x,
                        y: pos.y,
                    }),
                    None => None,
                }) {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
        }
        if req.delete_boresight.unwrap_or(false) {
            let solve_engine = &mut locked_state.solve_engine.lock().await;
            match solve_engine.set_target_pixel(None) {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
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
            return Err(tonic::Status::unimplemented(
                "ActionRequest.stop_slew not yet implemented."));
        }
        if req.save_image.unwrap_or(false) {
            let solve_engine = &mut locked_state.solve_engine.lock().await;
            match solve_engine.save_image().await {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
        }
        Ok(tonic::Response::new(EmptyMessage{}))
    }
}

impl MyCedar {
    async fn set_exposure_time(state: &CedarState, exposure_time: std::time::Duration)
                               -> Result<(), CanonicalError> {
        state.detect_engine.lock().await.set_exposure_time(exposure_time).await
    }

    async fn set_detection_max_size(state: &CedarState, max_size: i32)
                                    -> Result<(), CanonicalError> {
        state.detect_engine.lock().await.set_max_size(max_size)
    }

    async fn set_update_interval(state: &CedarState, update_interval: std::time::Duration)
                                 -> Result<(), CanonicalError> {
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
        locked_camera.set_offset(Offset::new(3))?;

        let mut locked_solve_engine = state.solve_engine.lock().await;
        locked_solve_engine.set_fov_estimate(/*fov_estimate=*/None)?;
        locked_solve_engine.set_distortion(0.0)?;
        locked_solve_engine.set_solve_timeout(Duration::from_secs(1))?;
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
        let detection_sigma;
        let detection_max_size;
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
            detection_sigma = locked_state.base_detection_sigma;
            detection_max_size =
                locked_state.operation_settings.lock().unwrap().detection_max_size.unwrap();
            star_count_goal = detect_engine.lock().await.get_star_count_goal();
        }
        let offset = match calibrator.lock().await.calibrate_offset(
            cancel_calibration.clone()).await
        {
            Ok(o) => o,
            Err(e) => {
                if e.code == CanonicalErrorCode::Aborted {
                    return Err(e);
                }
                warn!{"Error while calibrating offset: {:?}, using 3", e};
                Offset::new(3)  // Sane fallback value.
            }
        };
        camera.lock().await.set_offset(offset)?;
        calibration_data.lock().await.camera_offset = Some(offset.value());

        let exp_duration = match calibrator.lock().await.calibrate_exposure_duration(
            setup_exposure_duration, star_count_goal, detection_sigma,
            detection_max_size, cancel_calibration.clone()).await {
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
            solve_engine.clone(), exp_duration, solve_timeout, detection_sigma,
            detection_max_size).await
        {
            Ok((fov, distortion, solve_duration)) => {
                let mut locked_calibration_data = calibration_data.lock().await;
                locked_calibration_data.fov_horizontal = Some(fov);
                locked_calibration_data.lens_distortion = Some(distortion);
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
                        Duration::from_secs(2));  // TODO: max solve time cmd line arg
                let mut locked_solve_engine = solve_engine.lock().await;
                locked_solve_engine.set_fov_estimate(Some(fov))?;
                locked_solve_engine.set_distortion(distortion)?;
                locked_solve_engine.set_solve_timeout(operation_solve_timeout)?;
            }
            Err(e) => {
                let mut locked_calibration_data = calibration_data.lock().await;
                locked_calibration_data.fov_horizontal = None;
                locked_calibration_data.lens_distortion = None;
                let mut locked_solve_engine = solve_engine.lock().await;
                locked_solve_engine.set_fov_estimate(None)?;
                locked_solve_engine.set_distortion(0.0)?;
                // TODO: pass this in? Should come from command line, maybe is
                // max solve time.
                locked_solve_engine.set_solve_timeout(Duration::from_secs(1))?;
                if e.code == CanonicalErrorCode::Aborted {
                    return Err(e);
                }
                warn!{"Error while calibrating optics: {:?}", e};
            }
        };
        info!("Calibration result: {:?}", calibration_data.lock().await);
        Ok(())
    }

    async fn get_next_frame(state: Arc<tokio::sync::Mutex<CedarState>>,
                            prev_frame_id: Option<i32>, main_image_mode: i32)
                            -> FrameResult {
        let overall_start_time = Instant::now();

        let mut binning_factor = 1;
        if main_image_mode == ImageMode::Binned as i32 {
            binning_factor = 2;
        }

        let mut frame_result = FrameResult {..Default::default()};

        if state.lock().await.calibrating {
            let locked_state = state.lock().await;
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
                let image_rectangle = Rectangle{
                    origin_x: 0, origin_y: 0,
                    width: locked_state.width as i32,
                    height: locked_state.height as i32,
                };
                let (scaled_width, scaled_height) = img.dimensions();
                let mut bmp_buf = Vec::<u8>::new();
                bmp_buf.reserve((scaled_width * scaled_height) as usize);
                img.write_to(&mut Cursor::new(&mut bmp_buf),
                             ImageOutputFormat::Bmp).unwrap();
                frame_result.image = Some(Image{
                    binning_factor,
                    // Rectangle is always in full resolution coordinates.
                    rectangle: Some(image_rectangle),
                    image_data: bmp_buf,
                });
            }
            return frame_result;
        }

        // Populated only in OperatingMode::Operate mode.
        let mut tetra3_solve_result: Option<SolveResultProto> = None;
        let mut plate_solution: Option<PlateSolution> = None;

        let detect_result;
        if state.lock().await.operation_settings.lock().unwrap().operating_mode.unwrap() ==
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

        frame_result.frame_id = detect_result.frame_id;
        let captured_image = &detect_result.captured_image;
        let (width, height) = captured_image.image.dimensions();
        state.lock().await.width = width;
        state.lock().await.height = height;
        let image_rectangle = Rectangle{
            origin_x: 0, origin_y: 0,
            width: width as i32,
            height: height as i32,
        };
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

        let peak_value;
        if let Some(fa) = &detect_result.focus_aid {
            peak_value = fa.center_peak_value;
            frame_result.center_region = Some(Rectangle {
                origin_x: fa.center_region.left(),
                origin_y: fa.center_region.top(),
                width: fa.center_region.width() as i32,
                height: fa.center_region.height() as i32});

            let ic = ImageCoord {
                x: fa.center_peak_position.0 as f32,
                y: fa.center_peak_position.1 as f32,
            };
            *state.lock().await.center_peak_position.lock().unwrap() = Some(ic.clone());
            frame_result.center_peak_position = Some(ic);
            frame_result.center_peak_value = Some(fa.center_peak_value as i32);

            // Populate `center_peak_image`.
            let mut center_peak_bmp_buf = Vec::<u8>::new();
            let center_peak_image = &fa.peak_image;
            let peak_image_region = &fa.peak_image_region;
            let (center_peak_width, center_peak_height) =
                center_peak_image.dimensions();
            center_peak_bmp_buf.reserve(
                (center_peak_width * center_peak_height) as usize);
            center_peak_image.write_to(&mut Cursor::new(&mut center_peak_bmp_buf),
                                       ImageOutputFormat::Bmp).unwrap();
            frame_result.center_peak_image = Some(Image{
                binning_factor: 1,
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
            *state.lock().await.center_peak_position.lock().unwrap() = None;
        }

        // Populate `image` if requested.
        let mut disp_image = &captured_image.image;
        if main_image_mode == ImageMode::Binned as i32 {
            disp_image = &detect_result.binned_image;
        }
        if main_image_mode != ImageMode::Omit as i32 {
            let mut bmp_buf = Vec::<u8>::new();
            let (width, height) = disp_image.dimensions();
            bmp_buf.reserve((width * height) as usize);
            let scaled_image = scale_image(disp_image, peak_value, /*gamma=*/0.7);
            // Save most recent display image.
            state.lock().await.scaled_image = Some(Arc::new(scaled_image.clone()));
            scaled_image.write_to(&mut Cursor::new(&mut bmp_buf),
                                  ImageOutputFormat::Bmp).unwrap();
            frame_result.image = Some(Image{
                binning_factor,
                // Rectangle is always in full resolution coordinates.
                rectangle: Some(image_rectangle),
                image_data: bmp_buf,
            });
        }

        let mut locked_state = state.lock().await;
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
        }
        if tetra3_solve_result.is_some() {
            frame_result.plate_solution = Some(tetra3_solve_result.unwrap());
        }
        let boresight_position =
            locked_state.solve_engine.lock().await.target_pixel().expect(
                "solve_engine.target_pixel() should not fail");
        frame_result.boresight_position = match boresight_position {
            Some(bs) => Some(ImageCoord{x: bs.x, y: bs.y}),
            None => None,
        };
        frame_result.calibration_data =
            Some(locked_state.calibration_data.lock().await.clone());
        frame_result.operation_settings =
            Some(locked_state.operation_settings.lock().unwrap().clone());

        // TODO: slew_request from position_reporter.

        frame_result
    }

    pub async fn new(min_exposure_duration: Duration,
                     max_exposure_duration: Duration,
                     tetra3_script: String,
                     tetra3_database: String,
                     tetra3_uds: String,
                     camera: Arc<tokio::sync::Mutex<dyn AbstractCamera + Send>>,
                     telescope_position: Arc<Mutex<TelescopePosition>>,
                     base_star_count_goal: i32,
                     base_detection_sigma: f32,
                     min_detection_sigma: f32,
                     stats_capacity: usize) -> Self {
        let detect_engine = Arc::new(tokio::sync::Mutex::new(DetectEngine::new(
            min_exposure_duration,
            max_exposure_duration,
            camera.clone(),
            /*update_interval=*/Duration::ZERO,
            /*auto_exposure=*/true,
            /*focus_mode_enabled=*/true,
            stats_capacity)));
        let tetra3_subprocess = Arc::new(Mutex::new(
            Tetra3Subprocess::new(tetra3_script, tetra3_database).unwrap()));
        let state = Arc::new(tokio::sync::Mutex::new(CedarState {
            camera: camera.clone(),
            fixed_settings: Mutex::new(FixedSettings {
                observer_location: None,
                client_time: None,
                session_name: None,
            }),
            operation_settings: Mutex::new(OperationSettings {
                operating_mode: Some(OperatingMode::Setup as i32),
                exposure_time: Some(prost_types::Duration {
                    seconds: 0, nanos: 0,
                }),
                accuracy: Some(Accuracy::Balanced.into()),
                // TODO: command line arg for detection_max_size. Or
                // figure out how to calibrate it.
                detection_max_size: Some(10),
                update_interval: Some(prost_types::Duration {
                    seconds: 0, nanos: 0,
                }),
                dwell_update_interval: Some(prost_types::Duration {
                    seconds: 1, nanos: 0,
                }),
                log_dwelled_positions: Some(false),
            }),
            calibration_data: Arc::new(tokio::sync::Mutex::new(
                CalibrationData{..Default::default()})),
            detect_engine: detect_engine.clone(),
            tetra3_subprocess: tetra3_subprocess.clone(),
            solve_engine: Arc::new(tokio::sync::Mutex::new(SolveEngine::new(
                tetra3_subprocess.clone(), detect_engine.clone(),
                telescope_position.clone(), tetra3_uds,
                /*update_interval=*/Duration::ZERO,
                stats_capacity).await.unwrap())),
            calibrator: Arc::new(tokio::sync::Mutex::new(
                Calibrator::new(camera.clone()))),
            scaled_image: None,
            width: 0,
            height: 0,
            calibrating: false,
            cancel_calibration: Arc::new(Mutex::new(false)),
            calibration_start: Instant::now(),
            calibration_duration_estimate: Duration::MAX,
            base_star_count_goal,
            base_detection_sigma,
            min_detection_sigma,
            center_peak_position: Arc::new(Mutex::new(None)),
            serve_latency_stats: ValueStatsAccumulator::new(stats_capacity),
            overall_latency_stats: ValueStatsAccumulator::new(stats_capacity),
        }));
        let cedar = MyCedar {
            state: state.clone(),
        };
        // Set pre-calibration defaults on camera.
        let locked_state = state.lock().await;
        match Self::set_pre_calibration_defaults(&*locked_state).await {
            Ok(()) => (),
            Err(x) => {
                warn!("Could not set default settings on camera {:?}", x)
            }
        }
        Self::set_detection_max_size(
            &*locked_state,
            locked_state.operation_settings.lock().unwrap().detection_max_size.unwrap())
            .await.unwrap();
        Self::update_accuracy_adjusted_params(&*locked_state).await;

        cedar
    }

    async fn update_accuracy_adjusted_params(state: &CedarState) {
        let accuracy: i32 = state.operation_settings.lock().unwrap().accuracy.unwrap();
        // https://stackoverflow.com/questions/28028854/how-do-i-match-enum-values-with-an-integer
        let acc_enum: Accuracy = unsafe { ::std::mem::transmute(accuracy) };
        let multiplier = match acc_enum {
            Accuracy::Fastest => 0.5,
            Accuracy::Faster => 0.7,
            Accuracy::Balanced => 1.0,
            Accuracy::Accurate => 1.4,
            _ => 1.0,
        };
        let mut locked_detect_engine = state.detect_engine.lock().await;
        locked_detect_engine.set_star_count_goal(
            (state.base_star_count_goal as f32 * multiplier) as i32);

        let mut sigma = state.base_detection_sigma as f32 * multiplier;
        if sigma < state.min_detection_sigma {
            sigma = state.min_detection_sigma;
        }
        locked_detect_engine.set_sigma(sigma).unwrap();

        // In setup mode, we aim auto-exposure towards a value lower than 255, to allow
        // exposure times to be faster. The accuracy multiplier is used to raise or lower
        // this value.
        let base_brightness_goal = 128;  // Probably don't need a command line arg for this.
        let mut adjusted_brightness_goal = base_brightness_goal as f32 * multiplier;
        if adjusted_brightness_goal > 255.0 {
            adjusted_brightness_goal = 255.0;
        }
        locked_detect_engine.set_brightness_goal(adjusted_brightness_goal as u8);
    }
}

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

    /// Unix domain socket file for Tetra3 gRPC server.
    #[arg(long, default_value = "/home/pi/tetra3.sock")]
    tetra3_socket: String,

    /// Test image to use instead of camera.
    #[arg(long, default_value = "")]
    test_image: String,

    /// Minimum exposure duration, seconds.
    #[arg(long, value_parser = parse_duration, default_value = "0.00001")]
    min_exposure: Duration,

    /// Maximum exposure duration, seconds.
    #[arg(long, value_parser = parse_duration, default_value = "1.0")]
    max_exposure: Duration,

    /// Target number of detected stars for auto-exposure. This is altered by
    /// the OperationSettings.accuracy setting (multiplier ranging from 0.5 to
    /// 1.4).
    #[arg(long, default_value = "30")]
    star_count_goal: i32,

    /// The S/N factor used to determine if a background-subtracted pixel is
    /// bright enough relative to the noise measure to be considered part of a
    /// star. This is altered by the OperationSettings.accuracy setting
    /// (multiplier ranging from 0.5 to 1.4).
    #[arg(long, default_value = "8.0")]
    sigma: f32,

    /// Specifies a value below which `sigma` is not adjusted by the
    /// OperationSettings.accuracy setting.
    #[arg(long, default_value = "5.0")]
    min_sigma: f32,

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
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    info!("Using Tetra3 server {:?} listening at {:?}", args.tetra3_script, args.tetra3_socket);

    // Build the static content web service.
    let rest = Router::new().nest_service(
        "/", ServeDir::new("/home/pi/projects/cedar/cedar_flutter/build/web"));

    // TODO(smr): discovery/enumeration mechanism for cameras. Or command
    // line arg?
    let camera: Arc<tokio::sync::Mutex<dyn AbstractCamera + Send>> =
        match args.test_image.as_str() {
        "" => Arc::new(tokio::sync::Mutex::new(asi_camera::ASICamera::new(
            asi_camera2::asi_camera2_sdk::ASICamera::new(0)).unwrap())),
        _ => {
            let input_path = PathBuf::from(&args.test_image);
            let img = ImageReader::open(&input_path).unwrap().decode().unwrap();
            let img_u8 = img.to_luma8();
            info!("Using test image {} instead of camera.", args.test_image);
            Arc::new(tokio::sync::Mutex::new(ImageCamera::new(img_u8).unwrap()))
        },
    };

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
    let grpc = tonic::transport::Server::builder()
        .accept_http1(true)
        .layer(GrpcWebLayer::new())
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any))
        .add_service(CedarServer::new(MyCedar::new(args.min_exposure,
                                                   args.max_exposure,
                                                   args.tetra3_script,
                                                   args.tetra3_database,
                                                   args.tetra3_socket,
                                                   camera,
                                                   shared_telescope_position.clone(),
                                                   args.star_count_goal,
                                                   args.sigma,
                                                   args.min_sigma,
                                                   // TODO: arg for this?
                                                   /*stats_capacity=*/100).await))
        .into_service();

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
