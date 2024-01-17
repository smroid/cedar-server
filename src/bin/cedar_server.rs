// Adapted from
// https://github.com/tokio-rs/axum/tree/main/examples/rest-grpc-multiplex
// https://github.com/tokio-rs/axum/blob/main/examples/static-file-server

use std::io::Cursor;
use std::net::SocketAddr;
use std::pin::Pin;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration};

use camera_service::abstract_camera::{AbstractCamera, Gain, Offset};
use camera_service::asi_camera;
use camera_service::image_camera::ImageCamera;
use canonical_error::{CanonicalError, CanonicalErrorCode};
use image::ImageOutputFormat;
use image::io::Reader as ImageReader;

use clap::Parser;
use axum::Router;
use futures::StreamExt;
use log::{info, warn};
use tower_http::{services::ServeDir, cors::CorsLayer, cors::Any};
use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic_web::GrpcWebLayer;
use tracing_subscriber;

use futures::join;

use cedar::cedar::cedar_server::{Cedar, CedarServer};
use cedar::cedar::{ActionRequest, CalibrationPhase,
                   EmptyMessage, FixedSettings, FrameRequest, FrameResult,
                   Image, ImageCoord, ImageMode, OperatingMode, OperationSettings,
                   ProcessingStats, Rectangle, StarCentroid};
use ::cedar::detect_engine::DetectEngine;
use ::cedar::solve_engine::SolveEngine;
use ::cedar::position_reporter::{CelestialPosition, create_alpaca_server};
use ::cedar::tetra3_subprocess::Tetra3Subprocess;
use ::cedar::value_stats::ValueStatsAccumulator;
use cedar::tetra3_server;

use self::multiplex_service::MultiplexService;

type ResponseStream =
    Pin<Box<dyn Stream<Item = Result<FrameResult, tonic::Status>> + Send>>;

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

struct State {
    camera: Arc<Mutex<dyn AbstractCamera>>,
    fixed_settings: Mutex<FixedSettings>,
    operation_settings: Mutex<OperationSettings>,
    // calibration_data: Mutex<CalibrationData>,
    detect_engine: Arc<Mutex<DetectEngine>>,
    solve_engine: Arc<Mutex<SolveEngine>>,
    position: Arc<Mutex<CelestialPosition>>,
    _tetra3_subprocess: Tetra3Subprocess,
    // TODO: calibration_engine.

    // For boresight capturing.
    center_peak_position: Arc<Mutex<Option<ImageCoord>>>,

    overall_latency_stats: Mutex<ValueStatsAccumulator>,
}

struct MyCedar {
    state: Arc<State>,
}

#[tonic::async_trait]
impl Cedar for MyCedar {
    // TODO: get_server_information RPC.

    async fn update_fixed_settings(
        &self, request: tonic::Request<FixedSettings>)
        -> Result<tonic::Response<FixedSettings>, tonic::Status>
    {
        let req: FixedSettings = request.into_inner();
        if req.latitude.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateFixedSettings not implemented for latitude."));
        }
        if req.longitude.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateFixedSettings not implemented for longitude."));
        }
        if req.client_time.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateFixedSettings not implemented for client_time."));
        }
        if req.session_name.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateFixedSettings not implemented for session_name."));
        }
        Ok(tonic::Response::new(self.state.fixed_settings.lock().unwrap().clone()))
    }

    async fn update_operation_settings(
        &self, request: tonic::Request<OperationSettings>)
        -> Result<tonic::Response<OperationSettings>, tonic::Status>
    {
        let req: OperationSettings = request.into_inner();
        if req.camera_gain.is_some() {
            let gain = req.camera_gain.unwrap();
            if gain < 0 || gain > 100 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got invalid gain: {}.", gain)));
            }
            match self.set_camera_gain(gain) {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.state.operation_settings.lock().unwrap().camera_gain = Some(gain);
        }
        if req.camera_offset.is_some() {
            let offset = req.camera_offset.unwrap();
            if offset < 0 || offset > 20 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got invalid offset: {}.", offset)));
            }
            match self.set_camera_offset(offset) {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.state.operation_settings.lock().unwrap().camera_offset = Some(offset);
        }
        if req.operating_mode.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for operating_mode."));
        }
        if req.exposure_time.is_some() {
            let exp_time = req.exposure_time.unwrap();
            if exp_time.seconds < 0 || exp_time.nanos < 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got negative exposure_time: {}.", exp_time)));
            }
            let std_duration = std::time::Duration::try_from(exp_time.clone()).unwrap();
            match self.set_exposure_time(std_duration) {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.state.operation_settings.lock().unwrap().exposure_time = Some(exp_time);
        }
        if req.detection_sigma.is_some() {
            let sigma = req.detection_sigma.unwrap();
            if sigma < 0.0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got negative detection_sigma: {}.", sigma)));
            }
            match self.set_detection_sigma(sigma) {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.state.operation_settings.lock().unwrap().detection_sigma = Some(sigma);
        }
        if req.detection_max_size.is_some() {
            let max_size = req.detection_max_size.unwrap();
            if max_size <= 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got non-positive detection_max_size: {}.", max_size)));
            }
            match self.set_detection_max_size(max_size) {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.state.operation_settings.lock().unwrap().detection_max_size = Some(max_size);
        }
        if req.update_interval.is_some() {
            let update_interval = req.update_interval.unwrap();
            if update_interval.seconds < 0 || update_interval.nanos < 0 {
                return Err(tonic::Status::invalid_argument(
                    format!("Got negative update_interval: {}.", update_interval)));
            }
            let std_duration = std::time::Duration::try_from(update_interval.clone()).unwrap();
            match self.set_update_interval(std_duration) {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.state.operation_settings.lock().unwrap().update_interval = Some(update_interval);
        }
        if req.dwell_update_interval.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for dwell_update_interval."));
        }
        if req.log_dwelled_positions.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for log_dwelled_positions."));
        }

        Ok(tonic::Response::new(self.state.operation_settings.lock().unwrap().clone()))
    }

    async fn get_frame(&self, request: tonic::Request<FrameRequest>)
                       -> Result<tonic::Response<FrameResult>, tonic::Status> {
        let req: FrameRequest = request.into_inner();
        let prev_frame_id = req.prev_frame_id;
        let main_image_mode = req.main_image_mode;

        // TODO: what should we do with the clien't RPC timeout? Perhaps our
        // call to get_next_frame() can be passed a deadline?

        let frame_result = Self::get_next_frame(
            &self.state, prev_frame_id, main_image_mode);
        Ok(tonic::Response::new(frame_result))
    }  // get_frame().

    type GetFramesStream = ResponseStream;
    async fn get_frames(&self, request: tonic::Request<FrameRequest>)
                        -> Result<tonic::Response<Self::GetFramesStream>,
                                  tonic::Status>
    {
        info!("Client connected from: {:?}", request.remote_addr());
        let state = self.state.clone();
        let req: FrameRequest = request.into_inner();
        let mut prev_frame_id = req.prev_frame_id;
        let main_image_mode = req.main_image_mode;

        // Adapted from
        // https://github.com/hyperium/tonic/blob/master/examples/src/streaming/server.rs

        // Yield infinite stream of frame results, as they are plate-solved.
        let repeat = std::iter::repeat_with(move || {
            let frame_result = Self::get_next_frame(
                &state, prev_frame_id, main_image_mode);
            prev_frame_id = Some(frame_result.frame_id);
            frame_result
        });
        let mut stream = Box::pin(tokio_stream::iter(repeat));

        // spawn() and channel() are required to handle client "disconnect"
        // functionality. The `output_stream` will not be polled after client
        // disconnect.
        // Use shallow channel depth to limit streaming latency.
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            while let Some(frame_result) = stream.next().await {
                match tx.send(Result::<_, tonic::Status>::Ok(frame_result)).await {
                    Ok(_) => {
                        // frame_result was queued to be send to client.
                    }
                    Err(_item) => {
                        // `output_stream` was built from rx and both are dropped.
                        break;
                    }
                }
            }
            info!("Client disconnected.");
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(tonic::Response::new(
            Box::pin(output_stream) as Self::GetFramesStream
        ))
    }

    async fn initiate_action(&self, request: tonic::Request<ActionRequest>)
                             -> Result<tonic::Response<EmptyMessage>, tonic::Status> {
        let req: ActionRequest = request.into_inner();
        let state = &self.state;
        if req.capture_boresight.unwrap_or(false) {
            let operating_mode =
                state.operation_settings.lock().unwrap().operating_mode.or(
                    Some(OperatingMode::Setup as i32)).unwrap();
            if operating_mode != OperatingMode::Setup as i32 {
                return Err(tonic::Status::failed_precondition(
                    format!("Not in Setup mode: {:?}.", operating_mode)));
            }
            let solve_engine = &mut state.solve_engine.lock().unwrap();
            let cpp = state.center_peak_position.lock().unwrap();
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
            let solve_engine = &mut state.solve_engine.lock().unwrap();
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
            let detect_engine = &mut state.detect_engine.lock().unwrap();
            let solve_engine = &mut state.solve_engine.lock().unwrap();
            detect_engine.reset_session_stats();
            solve_engine.reset_session_stats();
            state.overall_latency_stats.lock().unwrap().reset_session();
        }
        if req.save_image.unwrap_or(false) {
            let solve_engine = &mut state.solve_engine.lock().unwrap();
            match solve_engine.save_image() {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
        }
        Ok(tonic::Response::new(EmptyMessage{}))
    }
}

impl MyCedar {
    fn set_camera_gain(&self, gain: i32) -> Result<(), CanonicalError> {
        let mut locked_camera = self.state.camera.lock().unwrap();
        return locked_camera.set_gain(Gain::new(gain));
    }
    fn set_camera_offset(&self, offset: i32) -> Result<(), CanonicalError> {
        let mut locked_camera = self.state.camera.lock().unwrap();
        return locked_camera.set_offset(Offset::new(offset));
    }

    fn set_exposure_time(&self, exposure_time: std::time::Duration)
                         -> Result<(), CanonicalError> {
        let detect_engine = &mut self.state.detect_engine.lock().unwrap();
        detect_engine.set_exposure_time(exposure_time)
    }

    fn set_detection_sigma(&self, detection_sigma: f32)
                           -> Result<(), CanonicalError> {
        let state = &self.state;
        let detect_engine = &mut state.detect_engine.lock().unwrap();
        // TODO(smr): if `detection_sigma` is 0, we use calibration_data's `detection_sigma`
        // value.
        detect_engine.set_detection_params(
            detection_sigma,
            state.operation_settings.lock().unwrap().detection_max_size.unwrap())
    }
    fn set_detection_max_size(&self, max_size: i32)
                              -> Result<(), CanonicalError> {
        let state = &self.state;
        let detect_engine = &mut state.detect_engine.lock().unwrap();
        detect_engine.set_detection_params(
            state.operation_settings.lock().unwrap().detection_sigma.unwrap(),
            max_size)
    }

    fn set_update_interval(&self, update_interval: std::time::Duration)
                         -> Result<(), CanonicalError> {
        let state = &self.state;
        let detect_engine = &mut state.detect_engine.lock().unwrap();
        let solve_engine = &mut state.solve_engine.lock().unwrap();
        detect_engine.set_update_interval(update_interval)?;
        solve_engine.set_update_interval(update_interval)
    }

    fn get_next_frame(state: &State, prev_frame_id: Option<i32>, main_image_mode: i32)
                      -> FrameResult {
        // TODO(smr): do according to operating mode.
        let tetra3_solve_result;
        let detect_result;
        let plate_solution;
        let boresight_position;
        let solve_finish_time;
        {
            let solve_engine = &mut state.solve_engine.lock().unwrap();
            plate_solution = solve_engine.get_next_result(prev_frame_id);
            tetra3_solve_result = plate_solution.tetra3_solve_result;
            detect_result = plate_solution.detect_result;
            boresight_position = solve_engine.target_pixel().expect(
                "solve_engine.target_pixel() should not fail");
            solve_finish_time = plate_solution.solve_finish_time;
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
            operation_settings: Some(state.operation_settings.lock().unwrap().clone()),
            image: None,  // Is set below.
            star_candidates: centroids,
            hot_pixel_count: detect_result.hot_pixel_count,
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
                    *state.center_peak_position.lock().unwrap() = Some(ic.clone());
                    Some(ic)
                },
                None => {
                    *state.center_peak_position.lock().unwrap() = None;
                    None
                },
            },
            center_peak_image: None,  // Is set below.
            calibration_phase: CalibrationPhase::None as i32,
            calibration_progress: None,
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
            let center_peak_image = &detect_result.focus_aid.as_ref().unwrap().peak_image;
            let peak_image_region = &detect_result.focus_aid.as_ref().unwrap().peak_image_region;
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
        let mut position = state.position.lock().unwrap();
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
            match solve_finish_time.unwrap().duration_since(captured_image.readout_time) {
                Err(e) => {
                    warn!("Clock may have gone backwards: {:?}", e)
                },
                Ok(d) => {
                    state.overall_latency_stats.lock().unwrap().add_value(
                        d.as_secs_f64());
                }
            }
            frame_result.plate_solution = Some(tetra3_solve_result.unwrap());
        }
        frame_result.processing_stats = Some(ProcessingStats {
            overall_latency: Some(
                state.overall_latency_stats.lock().unwrap().value_stats.clone()),
            detect_latency: Some(detect_result.detect_latency_stats),
            solve_interval: Some(plate_solution.solve_interval_stats),
            solve_latency: Some(plate_solution.solve_latency_stats),
            solve_attempt_fraction: Some(plate_solution.solve_attempt_stats),
            solve_success_fraction: Some(plate_solution.solve_success_stats),
        });

        frame_result
    }

    pub fn new(tetra3_script: String,
               tetra3_database: String,
               tetra3_uds: String,
               camera: Arc<Mutex<dyn AbstractCamera>>,
               position: Arc<Mutex<CelestialPosition>>,
               stats_capacity: usize) -> Self {
        let detect_engine = Arc::new(Mutex::new(DetectEngine::new(
            camera.clone(),
            /*update_interval=*/Duration::ZERO,
            /*exposure_time=*/Duration::ZERO,
            /*focus_mode_enabled=*/true,
            stats_capacity)));
        let solve_engine = Arc::new(Mutex::new(SolveEngine::new(
            detect_engine.clone(),
            tetra3_uds,
            /*update_interval=*/Duration::ZERO,
            stats_capacity)));
        let state = Arc::new(State {
            camera: camera.clone(),
            fixed_settings: Mutex::new(FixedSettings {
                latitude: None,
                longitude: None,
                client_time: None,
                session_name: None,
            }),
            operation_settings: Mutex::new(OperationSettings {
                camera_gain: None,
                camera_offset: None,
                operating_mode: Some(OperatingMode::Setup as i32),
                exposure_time: Some(prost_types::Duration {
                    seconds: 0, nanos: 0,
                }),
                detection_sigma: Some(8.0),
                detection_max_size: Some(5),
                update_interval: Some(prost_types::Duration {
                    seconds: 0, nanos: 0,
                }),
                dwell_update_interval: Some(prost_types::Duration {
                    seconds: 1, nanos: 0,
                }),
                log_dwelled_positions: Some(false),
            }),
            // calibration_data: Mutex::new(CalibrationData::default()),
            detect_engine: detect_engine.clone(),
            solve_engine: solve_engine.clone(),
            position,
            _tetra3_subprocess: Tetra3Subprocess::new(
                tetra3_script, tetra3_database).unwrap(),
            center_peak_position: Arc::new(Mutex::new(None)),
            overall_latency_stats: Mutex::new(ValueStatsAccumulator::new(stats_capacity)),
        });
        let cedar = MyCedar { state: state.clone() };
        // Set pre-calibration defaults on camera.
        match cedar.set_camera_gain(100) {
            Ok(()) => (),
            Err(x) => {
                warn!("Could not set gain on camera {:?}", x)
            }
        }
        match cedar.set_camera_offset(3) {
            Ok(()) => (),
            Err(x) => {
                warn!("Could not set offset on camera {:?}", x)
            }
        }
        let sigma;
        let max_size;
        {
            let op_settings = state.operation_settings.lock().unwrap();
            sigma = op_settings.detection_sigma.unwrap();
            max_size = op_settings.detection_max_size.unwrap();
        }
        assert!(detect_engine.lock().unwrap().set_detection_params(
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
    script: String,

    /// Star catalog database for Tetra3 to load.
    #[arg(long, default_value = "default_database")]
    database: String,

    /// Unix domain socket file for Tetra3 gRPC server.
    #[arg(long, default_value = "/home/pi/tetra3.sock")]
    socket: String,

    /// Test image to use instead of camera.
    #[arg(long, default_value = "")]
    test_image: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    info!("Using Tetra3 server {:?} listening at {:?}", args.script, args.socket);

    // Build the static content web service.
    let rest = Router::new().nest_service(
        "/", ServeDir::new("/home/pi/projects/cedar/cedar_flutter/build/web"));

    // TODO(smr): discovery/enumeration mechanism for cameras. Or command
    // line arg?
    let camera: Arc<Mutex<dyn AbstractCamera>> = match args.test_image.as_str() {
        "" => Arc::new(Mutex::new(asi_camera::ASICamera::new(
            asi_camera2::asi_camera2_sdk::ASICamera::new(0)).unwrap())),
        _ => {
            let input_path = PathBuf::from(&args.test_image);
            let img = ImageReader::open(&input_path).unwrap().decode().unwrap();
            let img_u8 = img.to_luma8();
            info!("Using test image {} instead of camera.", args.test_image);
            Arc::new(Mutex::new(ImageCamera::new(img_u8).unwrap()))
        },
    };

    let shared_position = Arc::new(Mutex::new(CelestialPosition::new()));

    // Build the gRPC service.
    let grpc = tonic::transport::Server::builder()
        .accept_http1(true)
        .layer(GrpcWebLayer::new())
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any))
        .add_service(CedarServer::new(MyCedar::new(args.script,
                                                   args.database,
                                                   args.socket,
                                                   camera,
                                                   shared_position.clone(),
                                                   /*stats_capacity=*/100)))
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
