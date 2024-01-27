use std::io::Cursor;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use camera_service::abstract_camera::{AbstractCamera, Offset};
use camera_service::asi_camera;
use camera_service::image_camera::ImageCamera;
use canonical_error::{CanonicalError, CanonicalErrorCode};
use image::ImageOutputFormat;
use image::io::Reader as ImageReader;

use clap::Parser;
use axum::Router;
use log::{info, warn};
use tower_http::{services::ServeDir, cors::CorsLayer, cors::Any};
use tonic_web::GrpcWebLayer;
use tracing_subscriber;

use futures::join;

use cedar::cedar::cedar_server::{Cedar, CedarServer};
use cedar::cedar::{ActionRequest, CalibrationData,
                   EmptyMessage, FixedSettings, FrameRequest, FrameResult,
                   Image, ImageCoord, ImageMode, OperatingMode, OperationSettings,
                   ProcessingStats, Rectangle, StarCentroid};
use ::cedar::calibrator::Calibrator;
use ::cedar::detect_engine::DetectEngine;
use ::cedar::solve_engine::{PlateSolution, SolveEngine};
use ::cedar::position_reporter::{CelestialPosition, create_alpaca_server};
use ::cedar::tetra3_subprocess::Tetra3Subprocess;
use ::cedar::value_stats::ValueStatsAccumulator;
use cedar::tetra3_server;
use cedar::tetra3_server::SolveResult as SolveResultProto;

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
    camera: Arc<tokio::sync::Mutex<dyn AbstractCamera + Send>>,
    fixed_settings: Mutex<FixedSettings>,
    calibration_data: tokio::sync::Mutex<CalibrationData>,
    operation_settings: Mutex<OperationSettings>,
    detect_engine: Arc<tokio::sync::Mutex<DetectEngine>>,
    _tetra3_subprocess: Tetra3Subprocess,
    solve_engine: Arc<tokio::sync::Mutex<SolveEngine>>,
    position: Arc<Mutex<CelestialPosition>>,
    calibrator: Arc<tokio::sync::Mutex<Calibrator>>,

    // For boresight capturing.
    center_peak_position: Arc<Mutex<Option<ImageCoord>>>,

    overall_latency_stats: Mutex<ValueStatsAccumulator>,
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
        Ok(tonic::Response::new(self.fixed_settings.lock().unwrap().clone()))
    }

    async fn update_operation_settings(
        &self, request: tonic::Request<OperationSettings>)
        -> Result<tonic::Response<OperationSettings>, tonic::Status>
    {
        let req: OperationSettings = request.into_inner();
        if req.operating_mode.is_some() {
            let operating_mode = req.operating_mode.unwrap();
            if operating_mode !=
                self.operation_settings.lock().unwrap().operating_mode.unwrap()
            {
                if operating_mode == OperatingMode::Setup as i32 {
                    match self.set_pre_calibration_defaults().await {
                        Ok(()) => {},
                        Err(x) => { return Err(tonic_status(x)); }
                    }
                    self.detect_engine.lock().await.set_focus_mode(true);
                    self.reset_session_stats().await;
                } else if operating_mode == OperatingMode::Operate as i32 {
                    match self.calibrate().await {
                        Ok(()) => {},
                        Err(x) => { return Err(tonic_status(x)); }
                    }
                    self.detect_engine.lock().await.set_focus_mode(false);
                } else {
                    return Err(tonic::Status::invalid_argument(
                        format!("Got invalid operating_mode: {}.", operating_mode)));
                }
                self.operation_settings.lock().unwrap().operating_mode =
                    Some(operating_mode);
            }
        }
        if req.exposure_time.is_some() {
            let exp_time = req.exposure_time.unwrap();
            if exp_time.seconds < 0 || exp_time.nanos < 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got negative exposure_time: {}.", exp_time)));
            }
            let std_duration = std::time::Duration::try_from(exp_time.clone()).unwrap();
            match self.set_exposure_time(std_duration).await {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.operation_settings.lock().unwrap().exposure_time = Some(exp_time);
        }
        if req.detection_sigma.is_some() {
            let sigma = req.detection_sigma.unwrap();
            if sigma < 0.0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got negative detection_sigma: {}.", sigma)));
            }
            match self.set_detection_sigma(sigma).await {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.operation_settings.lock().unwrap().detection_sigma = Some(sigma);
        }
        if req.detection_max_size.is_some() {
            let max_size = req.detection_max_size.unwrap();
            if max_size <= 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got non-positive detection_max_size: {}.", max_size)));
            }
            match self.set_detection_max_size(max_size).await {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.operation_settings.lock().unwrap().detection_max_size = Some(max_size);
        }
        if req.update_interval.is_some() {
            let update_interval = req.update_interval.unwrap();
            if update_interval.seconds < 0 || update_interval.nanos < 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got negative update_interval: {}.", update_interval)));
            }
            let std_duration = std::time::Duration::try_from(
                update_interval.clone()).unwrap();
            match self.set_update_interval(std_duration).await {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.operation_settings.lock().unwrap().update_interval =
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

        Ok(tonic::Response::new(self.operation_settings.lock().unwrap().clone()))
    }

    async fn get_frame(&self, request: tonic::Request<FrameRequest>)
                       -> Result<tonic::Response<FrameResult>, tonic::Status> {
        let req: FrameRequest = request.into_inner();
        let prev_frame_id = req.prev_frame_id;
        let main_image_mode = req.main_image_mode;

        // TODO: what should we do with the client's RPC timeout? Perhaps our
        // call to get_next_frame() can be passed a deadline?

        let frame_result = self.get_next_frame(prev_frame_id, main_image_mode).await;
        Ok(tonic::Response::new(frame_result))
    }  // get_frame().

    async fn initiate_action(&self, request: tonic::Request<ActionRequest>)
                             -> Result<tonic::Response<EmptyMessage>, tonic::Status> {
        let req: ActionRequest = request.into_inner();
        if req.capture_boresight.unwrap_or(false) {
            let operating_mode =
                self.operation_settings.lock().unwrap().operating_mode.or(
                    Some(OperatingMode::Setup as i32)).unwrap();
            if operating_mode != OperatingMode::Setup as i32 {
                return Err(tonic::Status::failed_precondition(
                    format!("Not in Setup mode: {:?}.", operating_mode)));
            }
            let solve_engine = &mut self.solve_engine.lock().await;
            let cpp = self.center_peak_position.lock().unwrap();
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
            let solve_engine = &mut self.solve_engine.lock().await;
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
        if req.reset_session_stats.unwrap_or(false) {
            self.reset_session_stats().await;
        }
        if req.save_image.unwrap_or(false) {
            let solve_engine = &mut self.solve_engine.lock().await;
            match solve_engine.save_image().await {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
        }
        Ok(tonic::Response::new(EmptyMessage{}))
    }
}

impl MyCedar {
    async fn set_exposure_time(&self, exposure_time: std::time::Duration)
                               -> Result<(), CanonicalError> {
        let detect_engine = &mut self.detect_engine.lock().await;
        detect_engine.set_exposure_time(exposure_time).await
    }

    async fn set_detection_sigma(&self, detection_sigma: f32)
                                 -> Result<(), CanonicalError> {
        let detect_engine = &mut self.detect_engine.lock().await;
        detect_engine.set_detection_params(
            detection_sigma,
            self.operation_settings.lock().unwrap().detection_max_size.unwrap())
    }
    async fn set_detection_max_size(&self, max_size: i32)
                                    -> Result<(), CanonicalError> {
        let detect_engine = &mut self.detect_engine.lock().await;
        detect_engine.set_detection_params(
            self.operation_settings.lock().unwrap().detection_sigma.unwrap(),
            max_size)
    }

    async fn set_update_interval(&self, update_interval: std::time::Duration)
                                 -> Result<(), CanonicalError> {
        {
            let detect_engine = &mut self.detect_engine.lock().await;
            detect_engine.set_update_interval(update_interval)?;
        }
        let solve_engine = &mut self.solve_engine.lock().await;
        solve_engine.set_update_interval(update_interval)
    }

    async fn reset_session_stats(&self) {
        self.detect_engine.lock().await.reset_session_stats();
        self.solve_engine.lock().await.reset_session_stats();
        self.overall_latency_stats.lock().unwrap().reset_session();
    }

    // Called when entering SETUP mode.
    async fn set_pre_calibration_defaults(&self) -> Result<(), CanonicalError> {
        {
            let mut locked_camera = self.camera.lock().await;
            let gain = locked_camera.optimal_gain();
            locked_camera.set_gain(gain)?;
            locked_camera.set_offset(Offset::new(3))?;
        }
        let mut locked_solve_engine = self.solve_engine.lock().await;
        locked_solve_engine.set_fov_estimate(/*fov_estimate=*/None,
                                             /*fov_max_error*/None)?;
        locked_solve_engine.set_distortion(0.0)?;
        locked_solve_engine.set_match_max_error(0.005)?;
        locked_solve_engine.set_solve_timeout(Duration::from_secs(1))?;
        *self.calibration_data.lock().await = CalibrationData{..Default::default()};
        Ok(())
    }

    // Called when entering OPERATE mode.
    async fn calibrate(&self) -> Result<(), CanonicalError> {
        let locked_calibrator = self.calibrator.lock().await;
        let mut locked_calibration_data = self.calibration_data.lock().await;
        locked_calibration_data.calibration_time = Some(prost_types::Timestamp::try_from(
                SystemTime::now()).unwrap());

        // What was the final exposure duration coming out of SETUP mode?
        let setup_exposure_duration = self.camera.lock().await.get_exposure_duration();

        let offset = match locked_calibrator.calibrate_offset().await {
            Ok(o) => o,
            Err(e) => {
                warn!{"Error while calibrating offset: {:?}, using 3", e};
                Offset::new(3)  // Sane fallback value.
            }
        };
        self.camera.lock().await.set_offset(offset)?;
        locked_calibration_data.camera_offset = Some(offset.value());

        let detection_sigma;
        let detection_max_size;
        {
            let op_settings = self.operation_settings.lock().unwrap();
            detection_sigma = op_settings.detection_sigma.unwrap();
            detection_max_size = op_settings.detection_max_size.unwrap();
        }
        let exp_duration = match locked_calibrator.calibrate_exposure_duration(
            setup_exposure_duration,
            self.detect_engine.lock().await.get_star_count_goal(),
            detection_sigma, detection_max_size).await {
            Ok(ed) => ed,
            Err(e) => {
                warn!{"Error while calibrating exposure duration: {:?}, using {:?}",
                      e, setup_exposure_duration};
                setup_exposure_duration  // Sane fallback value.
            }
        };
        self.camera.lock().await.set_exposure_duration(exp_duration)?;
        locked_calibration_data.target_exposure_time =
            Some(prost_types::Duration::try_from(exp_duration).unwrap());
        self.detect_engine.lock().await.set_calibrated_exposure_duration(
            exp_duration);

        match locked_calibrator.calibrate_optical(
            self.solve_engine.clone(), exp_duration,
            detection_sigma, detection_max_size).await
        {
            Ok((fov, distortion, solve_duration)) => {
                locked_calibration_data.fov_horizontal = Some(fov);
                locked_calibration_data.lens_distortion = Some(distortion);
                let sensor_width_mm = self.camera.lock().await.sensor_size().0;
                let lens_fl_mm =
                    sensor_width_mm / (2.0 * (fov/2.0).to_radians()).tan();
                locked_calibration_data.lens_fl_mm = Some(lens_fl_mm);
                let pixel_width_mm =
                    sensor_width_mm / self.camera.lock().await.dimensions().0 as f32;
                locked_calibration_data.pixel_angular_size =
                    Some((pixel_width_mm / lens_fl_mm).atan().to_degrees());

                let solve_timeout =
                    std::cmp::max(solve_duration * 10, Duration::from_millis(500));
                let mut locked_solve_engine = self.solve_engine.lock().await;
                locked_solve_engine.set_fov_estimate(Some(fov), Some(fov/10.0))?;
                locked_solve_engine.set_distortion(distortion)?;
                locked_solve_engine.set_match_max_error(0.002)?;
                locked_solve_engine.set_solve_timeout(solve_timeout)?;
            }
            Err(e) => {
                warn!{"Error while calibrating optics: {:?}", e};
                locked_calibration_data.fov_horizontal = None;
                locked_calibration_data.lens_distortion = None;
                let mut locked_solve_engine = self.solve_engine.lock().await;
                locked_solve_engine.set_fov_estimate(None, None)?;
                locked_solve_engine.set_distortion(0.0)?;
                locked_solve_engine.set_match_max_error(0.005)?;
                locked_solve_engine.set_solve_timeout(Duration::from_secs(1))?;
            }
        };
        info!("Calibration result: {:?}", locked_calibration_data);
        Ok(())
    }

    async fn get_next_frame(&self, prev_frame_id: Option<i32>, main_image_mode: i32)
                            -> FrameResult {
        // Always populated.
        let detect_result;
        let boresight_position;
        // Populated only in OperatingMode::Operate mode.
        let mut tetra3_solve_result: Option<SolveResultProto> = None;
        let mut plate_solution: Option<PlateSolution> = None;
        let mut solve_finish_time: Option<SystemTime> = None;

        // Be mutually exclusive with calibrate().
        let locked_calibration_data = self.calibration_data.lock().await;

        if self.operation_settings.lock().unwrap().operating_mode.unwrap() ==
            OperatingMode::Setup as i32
        {
            detect_result =
                self.detect_engine.lock().await.get_next_result(prev_frame_id).await;
            boresight_position = self.solve_engine.lock().await.target_pixel().expect(
                "solve_engine.target_pixel() should not fail");
        } else {
            let solve_engine = &mut self.solve_engine.lock().await;
            plate_solution = Some(solve_engine.get_next_result(prev_frame_id).await);
            tetra3_solve_result =
                plate_solution.as_ref().unwrap().tetra3_solve_result.clone();
            solve_finish_time = plate_solution.as_ref().unwrap().solve_finish_time;
            detect_result = plate_solution.as_ref().unwrap().detect_result.clone();
            boresight_position = solve_engine.target_pixel().expect(
                "solve_engine.target_pixel() should not fail");
        }

        let captured_image = &detect_result.captured_image;
        let (width, height) = captured_image.image.dimensions();
        let image_rectangle = Rectangle{
            origin_x: 0, origin_y: 0,
            width: width as i32,
            height: height as i32,
        };

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

        let mut frame_result = FrameResult {
            frame_id: detect_result.frame_id,
            calibration_data: Some(locked_calibration_data.clone()),
            operation_settings: Some(self.operation_settings.lock().unwrap().clone()),
            image: None,  // Is set below.
            star_candidates: centroids,
            exposure_time: Some(prost_types::Duration::try_from(
                captured_image.capture_params.exposure_duration).unwrap()),
            processing_stats: None,  // Is set below.
            capture_time: Some(prost_types::Timestamp::try_from(
                captured_image.readout_time).unwrap()),
            camera_temperature_celsius: captured_image.temperature.0 as f32,
            boresight_position: match boresight_position {
                Some(bs) => Some(ImageCoord{x: bs.x, y: bs.y}),
                None => None,
            },
            center_region: match &detect_result.focus_aid {
                Some(fa) => Some(Rectangle {
                    origin_x: fa.center_region.left(),
                    origin_y: fa.center_region.top(),
                    width: fa.center_region.width() as i32,
                    height: fa.center_region.height() as i32,
                }),
                None => None,
            },
            center_peak_position: match &detect_result.focus_aid {
                Some(fa) => {
                    let ic = ImageCoord {
                        x: fa.center_peak_position.0 as f32,
                        y: fa.center_peak_position.1 as f32,
                    };
                    *self.center_peak_position.lock().unwrap() = Some(ic.clone());
                    Some(ic)
                },
                None => {
                    *self.center_peak_position.lock().unwrap() = None;
                    None
                },
            },
            // These are set below.
            center_peak_image: None,
            plate_solution: None,
            camera_motion: None,
            ra_rate: None,
            dec_rate: None,
        };

        // Populate `image` if requested.
        if main_image_mode == ImageMode::Default as i32 {
            let mut main_bmp_buf = Vec::<u8>::new();
            let image = &captured_image.image;
            main_bmp_buf.reserve((width * height) as usize);
            image.write_to(&mut Cursor::new(&mut main_bmp_buf),
                           ImageOutputFormat::Bmp).unwrap();
            frame_result.image = Some(Image{
                binning_factor: 1,
                rectangle: Some(image_rectangle),
                image_data: main_bmp_buf,
            });
        } else if main_image_mode == ImageMode::Binned as i32 {
            let mut binned_bmp_buf = Vec::<u8>::new();
            let binned_image = &detect_result.binned_image;
            let (binned_width, binned_height) = binned_image.dimensions();
            binned_bmp_buf.reserve((binned_width * binned_height) as usize);
            binned_image.write_to(&mut Cursor::new(&mut binned_bmp_buf),
                                  ImageOutputFormat::Bmp).unwrap();
            frame_result.image = Some(Image{
                binning_factor: 2,
                // Rectangle is always in full resolution coordinates.
                rectangle: Some(image_rectangle),
                image_data: binned_bmp_buf,
            });
        }
        if detect_result.focus_aid.is_some() {
            // Populate `center_peak_image`.
            let mut center_peak_bmp_buf = Vec::<u8>::new();
            let center_peak_image =
                &detect_result.focus_aid.as_ref().unwrap().peak_image;
            let peak_image_region =
                &detect_result.focus_aid.as_ref().unwrap().peak_image_region;
            let (center_peak_width, center_peak_height) =
                center_peak_image.dimensions();
            center_peak_bmp_buf.reserve(
                (2 * center_peak_width * center_peak_height) as usize);
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
        }
        frame_result.processing_stats = Some(ProcessingStats {
            detect_latency: Some(detect_result.detect_latency_stats),
            ..Default::default()
        });
        let mut position = self.position.lock().unwrap();
        position.valid = false;
        if tetra3_solve_result.is_some() {
            let tsr = tetra3_solve_result.as_ref().unwrap();
            if tsr.image_center_coords.is_some() {
                let coords;
                if tsr.target_coords.len() > 0 {
                    coords = tsr.target_coords[0].clone();
                } else {
                    coords = tsr.image_center_coords.as_ref().unwrap().clone();
                }
                position.ra = coords.ra as f64;
                position.dec = coords.dec as f64;
                position.valid = true;
            }
            // Overall latency is time between image acquisition and completion
            // of the plate solve attempt.
            match solve_finish_time.unwrap().duration_since(
                captured_image.readout_time) {
                Err(e) => {
                    warn!("Clock may have gone backwards: {:?}", e)
                },
                Ok(d) => {
                    self.overall_latency_stats.lock().unwrap().add_value(
                        d.as_secs_f64());
                }
            }
            frame_result.plate_solution = Some(tetra3_solve_result.unwrap());

            let stats = &mut frame_result.processing_stats.as_mut().unwrap();
            let plate_solution = &plate_solution.as_ref().unwrap();
            stats.overall_latency =
                Some(self.overall_latency_stats.lock().unwrap().value_stats.clone());
            stats.solve_interval = Some(plate_solution.solve_interval_stats.clone());
            stats.solve_latency = Some(plate_solution.solve_latency_stats.clone());
            stats.solve_attempt_fraction =
                Some(plate_solution.solve_attempt_stats.clone());
            stats.solve_success_fraction =
                Some(plate_solution.solve_success_stats.clone());
        }

        frame_result
    }

    pub async fn new(min_exposure_duration: Duration,
                     max_exposure_duration: Duration,
                     tetra3_script: String,
                     tetra3_database: String,
                     tetra3_uds: String,
                     camera: Arc<tokio::sync::Mutex<dyn AbstractCamera + Send>>,
                     position: Arc<Mutex<CelestialPosition>>,
                     star_count_goal: i32,
                     stats_capacity: usize) -> Self {
        let detect_engine = Arc::new(tokio::sync::Mutex::new(DetectEngine::new(
            min_exposure_duration,
            max_exposure_duration,
            camera.clone(),
            /*update_interval=*/Duration::ZERO,
            /*auto_exposure=*/true,
            /*focus_mode_enabled=*/true,
            star_count_goal,
            stats_capacity)));
        let cedar = MyCedar {
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
                // TODO: command line args for detection_{sigma,max_size}. Or
                // figure out how to calibrate them.
                detection_sigma: Some(8.0),
                detection_max_size: Some(8),
                update_interval: Some(prost_types::Duration {
                    seconds: 0, nanos: 0,
                }),
                dwell_update_interval: Some(prost_types::Duration {
                    seconds: 1, nanos: 0,
                }),
                log_dwelled_positions: Some(false),
            }),
            calibration_data: tokio::sync::Mutex::new(
                CalibrationData{..Default::default()}),
            detect_engine: detect_engine.clone(),
            _tetra3_subprocess: Tetra3Subprocess::new(
                tetra3_script, tetra3_database).unwrap(),
            solve_engine: Arc::new(tokio::sync::Mutex::new(SolveEngine::new(
                detect_engine.clone(),
                tetra3_uds,
                /*update_interval=*/Duration::ZERO,
                stats_capacity).await.unwrap())),
            position,
            calibrator: Arc::new(tokio::sync::Mutex::new(
                Calibrator::new(camera.clone()))),
            center_peak_position: Arc::new(Mutex::new(None)),
            overall_latency_stats: Mutex::new(
                ValueStatsAccumulator::new(stats_capacity)),
        };
        // Set pre-calibration defaults on camera.
        match cedar.set_pre_calibration_defaults().await {
            Ok(()) => (),
            Err(x) => {
                warn!("Could not set default settings on camera {:?}", x)
            }
        }
        let sigma;
        let max_size;
        {
            let op_settings = cedar.operation_settings.lock().unwrap();
            sigma = op_settings.detection_sigma.unwrap();
            max_size = op_settings.detection_max_size.unwrap();
        }
        assert!(cedar.detect_engine.lock().await.set_detection_params(
            sigma, max_size).is_ok());
        cedar
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about=None)]
struct Args {
    /// Path to tetra3_server.py script. Either set this on
    /// command line or set up a symlink. Note that PYPATH must
    /// be set to include the tetra3.py library location.
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

    /// Target number of detected stars for auto-exposure.
    #[arg(long, default_value = "20")]
    star_count_goal: i32,
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

    let shared_position = Arc::new(Mutex::new(CelestialPosition::new()));

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
                                                   shared_position.clone(),
                                                   args.star_count_goal,
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
    let alpaca_server = create_alpaca_server(shared_position);
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
