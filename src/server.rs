// Adapted from
// https://github.com/tokio-rs/axum/tree/main/examples/rest-grpc-multiplex
// https://github.com/tokio-rs/axum/blob/main/examples/static-file-server

use self::multiplex_service::MultiplexService;

use camera_service::abstract_camera::AbstractCamera;
use camera_service::asi_camera;
use image::{GrayImage, ImageOutputFormat};

use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use axum::Router;
use env_logger;
use log::info;
use tower_http::{services::ServeDir, cors::CorsLayer, cors::Any};
use tonic_web::GrpcWebLayer;

use crate::cedar::image_server::{Image, ImageServer};
use crate::cedar::{ImageReply, ImageRequest};

mod multiplex_service;

pub mod cedar {
    // The string specified here must match the proto package name.
    tonic::include_proto!("cedar");
}

struct State {
    camera: asi_camera::ASICamera,
}

struct MyImage {
    state: Mutex<State>,
}

#[tonic::async_trait]
impl Image for MyImage {
    async fn get_image(&self, request: tonic::Request<ImageRequest>)
                       -> Result<tonic::Response<ImageReply>, tonic::Status> {
        let req_start = Instant::now();

        let state = &mut self.state.lock().unwrap();
        let camera = &mut state.camera;
        let (width, height) = camera.dimensions();

        // Receive camera data, encode to BMP.
        let captured_image = camera.capture_image().unwrap();
        let image = GrayImage::from_raw(width as u32, height as u32,
                                        captured_image.image_data).unwrap();
        let mut bmp_buf = Vec::<u8>::new();
        bmp_buf.reserve((2 * width * height) as usize);
        image.write_to(&mut Cursor::new(&mut bmp_buf), ImageOutputFormat::Bmp).unwrap();

        info!("Responding to request: {:?} after {:?}", request, req_start.elapsed());
        Ok(tonic::Response::new(cedar::ImageReply {
            width: width,
            height: height,
            image_data: bmp_buf,
        }))
    }
}

impl MyImage {
    pub fn new() -> Self {
        let mut camera = asi_camera::ASICamera::new(
            asi_camera2::asi_camera2_sdk::ASICamera::new(0)).unwrap();
        camera.set_exposure_duration(Duration::from_millis(5)).unwrap();
        MyImage {
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
        .add_service(ImageServer::new(MyImage::new()))
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
