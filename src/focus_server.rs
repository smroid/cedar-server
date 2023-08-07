// Adapted from
// https://github.com/tokio-rs/axum/tree/main/examples/rest-grpc-multiplex
// https://github.com/tokio-rs/axum/blob/main/examples/static-file-server

use self::multiplex_service::MultiplexService;

use camera_service::abstract_camera::AbstractCamera;
use camera_service::asi_camera;
use image::ImageOutputFormat;

use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

use axum::Router;
use env_logger;
use log::info;
use tower_http::{services::ServeDir, cors::CorsLayer, cors::Any};
use tonic_web::GrpcWebLayer;

use crate::cedar::cedar_server::{Cedar, CedarServer};
use crate::cedar::{CalibrationPhase, FrameRequest, FrameResult, Image, ImageMode,
                   ImageCoord, OperatingMode, OperationSettings, Rectangle,
                   StarCentroid};
use ::cedar::focus_engine::FocusEngine;

mod multiplex_service;

pub mod cedar {
    // The string specified here must match the proto package name.
    tonic::include_proto!("cedar");
}

struct State {
    camera: Arc<Mutex<asi_camera::ASICamera>>,
    focus_engine: FocusEngine,
}

struct MyCedar {
    state: Mutex<State>,
}

#[tonic::async_trait]
impl Cedar for MyCedar {
    async fn update_operation_settings(&self, request: tonic::Request<OperationSettings>)
                                       -> Result<tonic::Response<OperationSettings>,
                                                 tonic::Status>
    {
        Err(tonic::Status::unimplemented("rpc UpdateOperationSettings not implemented."))
    }

    async fn get_frame(&self, request: tonic::Request<FrameRequest>)
                       -> Result<tonic::Response<FrameResult>, tonic::Status> {
        let req_start = Instant::now();
        let prev_frame_id = request.into_inner().prev_frame_id;
        let main_image_mode = request.into_inner().main_image_mode;

        let state = &mut self.state.lock().unwrap();
        let focus_result = state.focus_engine.get_next_result(prev_frame_id);
        let captured_image = &focus_result.captured_image;

        info!("Responding to request: {:?} after {:?}", request, req_start.elapsed());
        // let exp_dur = prost_types::Duration::try_from(
        //     captured_image.capture_params.exposure_duration
        // );
        let mut frame_result = cedar::FrameResult {
            frame_id: focus_result.frame_id,
            operating_mode: OperatingMode::Setup as i32,
            image: None,
            star_candidates: Vec::<StarCentroid>::new(),
            star_candidate_count: 0,
            hot_pixel_count: 0,
            exposure_time: Some(prost_types::Duration::try_from(
                captured_image.capture_params.exposure_duration).unwrap()),
            result_update_interval: None,  // TODO.
            capture_time: Some(prost_types::Timestamp::try_from(
                captured_image.readout_time).unwrap()),
            camera_temperature_celsius: captured_image.temperature.0 as f32,
            center_region: Some(Rectangle{
                origin_x: focus_result.center_region.left(),
                origin_y: focus_result.center_region.top(),
                width: focus_result.center_region.width() as i32,
                height: focus_result.center_region.height() as i32,
            }),
            center_peak_position: Some(ImageCoord{
                x: focus_result.peak_position.0 as f32,
                y: focus_result.peak_position.1 as f32,
            }),
            center_peak_image: None,
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
            if main_image_mode == ImageMode::Default as i32 {
                let image = &captured_image.image;
                let (width, height) = image.dimensions();
                main_bmp_buf.reserve((2 * width * height) as usize);
                image.write_to(&mut Cursor::new(&mut main_bmp_buf),
                               ImageOutputFormat::Bmp).unwrap();
                frame_result.image = Some(Image{
                    binning_factor: 1,
                    rectangle: Some(Rectangle{
                        origin_x: 0,
                        origin_y: 0,
                        width: width as i32,
                        height: height as i32,
                    }),
                    image_data: main_bmp_buf,
                });
            }
        }
        // Populate `center_peak_image`.
        let mut center_peak_bmp_buf = Vec::<u8>::new();
        let center_peak_image = &focus_result.peak_image;
        let peak_image_region = &focus_result.peak_image_region;
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

        Ok(tonic::Response::new(frame_result))
    }  // get_frame().
}

impl MyCedar {


    pub fn new() -> Self {
        let mut camera = asi_camera::ASICamera::new(
            asi_camera2::asi_camera2_sdk::ASICamera::new(0)).unwrap();
        camera.set_exposure_duration(Duration::from_millis(5)).unwrap();
        MyCedar {
            state: Mutex::new(State{camera: camera})
        }
    }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("debug")).init();

    // Build the static content web service.
    let rest = Router::new().nest_service(
        "/", ServeDir::new("/home/pi/projects/cedar/cedar_webapp/build/web"));

    // Build the grpc service.
    let grpc = tonic::transport::Server::builder()
        .accept_http1(true)
        .layer(GrpcWebLayer::new())
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any))
        .add_service(CedarServer::new(MyCedar::new()))
        .into_service();

    // Combine them into one service.
    let service = MultiplexService::new(rest, grpc);

    // Listen on any address for the given port.
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    hyper::Server::bind(&addr)
        .serve(tower::make::Shared::new(service))
        .await
        .unwrap();
}
