// Adapted from
// https://github.com/tokio-rs/axum/tree/main/examples/rest-grpc-multiplex
// https://github.com/tokio-rs/axum/blob/main/examples/static-file-server

use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

use camera_service::abstract_camera::{AbstractCamera, Gain, Offset};
use camera_service::asi_camera;
use canonical_error::{CanonicalError, CanonicalErrorCode};
use image::ImageOutputFormat;

use axum::Router;
use env_logger;
use log::{debug};
use tower_http::{services::ServeDir, cors::CorsLayer, cors::Any};
use tonic_web::GrpcWebLayer;

use crate::cedar::cedar_server::{Cedar, CedarServer};
use crate::cedar::{CalibrationPhase, FrameRequest, FrameResult, Image, ImageMode,
                   ImageCoord, OperatingMode, OperationSettings, Rectangle,
                   StarCentroid};
use ::cedar::focus_engine::FocusEngine;

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

pub mod cedar {
    // The string specified here must match the proto package name.
    tonic::include_proto!("cedar");
}

struct MyCedar {
    camera: Arc<Mutex<asi_camera::ASICamera>>,
    operation_settings: Mutex<OperationSettings>,
    focus_engine: Mutex<FocusEngine>,
}

#[tonic::async_trait]
impl Cedar for MyCedar {
    async fn update_operation_settings(
        &self, request: tonic::Request<OperationSettings>)
        -> Result<tonic::Response<OperationSettings>, tonic::Status>
    {
        let req: OperationSettings = request.into_inner();
        if req.camera_gain.is_some() {
            let mut locked_camera = self.camera.lock().unwrap();
            match locked_camera.set_gain(Gain::new(req.camera_gain.unwrap())) {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.operation_settings.lock().unwrap().camera_gain = req.camera_gain;
        }
        if req.camera_offset.is_some() {
            let mut locked_camera = self.camera.lock().unwrap();
            match locked_camera.set_offset(Offset::new(req.camera_offset.unwrap())) {
                Ok(()) => (),
                Err(x) => { return Err(tonic_status(x)); }
            }
            self.operation_settings.lock().unwrap().camera_offset = req.camera_offset;
        }
        if req.operating_mode.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for operating_mode."));
        }
        if req.exposure_time.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for exposure_time."));
        }
        if req.stargate_sigma.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for stargate_sigma."));
        }
        if req.update_interval.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for update_interval."));
        }
        if req.dwell_update_interval.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for dwell_update_interval."));
        }
        if req.catalog_tracks_visible_sky.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for catalog_tracks_visible_sky."));
        }
        if req.log_dwelled_positions.is_some() {
            return Err(tonic::Status::unimplemented(
                "rpc UpdateOperationSettings not implemented for log_dwelled_positions."));
        }

        Ok(tonic::Response::new(self.operation_settings.lock().unwrap().clone()))
    }

    async fn get_frame(&self, request: tonic::Request<FrameRequest>)
                       -> Result<tonic::Response<FrameResult>, tonic::Status> {
        let req_start = Instant::now();
        let req: FrameRequest = request.into_inner();
        let prev_frame_id = req.prev_frame_id;
        let main_image_mode = req.main_image_mode;

        let focus_engine = &mut self.focus_engine.lock().unwrap();
        let focus_result = focus_engine.get_next_result(prev_frame_id);
        let captured_image = &focus_result.captured_image;

        let mut frame_result = cedar::FrameResult {
            frame_id: focus_result.frame_id,
            operating_mode: OperatingMode::Setup as i32,
            image: None,
            star_candidates: Vec::<StarCentroid>::new(),
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
            binned_star_candidate_count: 0,
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

        debug!("Responding to request: {:?} after {:?}", req, req_start.elapsed());
        Ok(tonic::Response::new(frame_result))
    }  // get_frame().
}

impl MyCedar {
    pub fn new(camera: Arc<Mutex<asi_camera::ASICamera>>) -> Self {
        MyCedar {
            camera: camera.clone(),
            operation_settings: Mutex::new(OperationSettings {
                camera_gain: Some(100),
                camera_offset: Some(0),
                operating_mode: Some(OperatingMode::Setup as i32),
                exposure_time: Some(prost_types::Duration {
                    seconds: 0, nanos: 0,
                }),
                stargate_sigma: Some(8.0),
                update_interval: Some(prost_types::Duration {
                    seconds: 0, nanos: 0,
                }),
                binning: Some(0),
                dwell_update_interval: Some(prost_types::Duration {
                    seconds: 1, nanos: 0,
                }),
                catalog_tracks_visible_sky: Some(false),
                log_dwelled_positions: Some(false),
            }),
            focus_engine: Mutex::new(FocusEngine::new(
                camera.clone(),
                /*update_interval=*/Duration::from_secs(0),
                /*exposure_time=*/None))
        }
    }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info")).init();

    // Build the static content web service.
    let rest = Router::new().nest_service(
        "/", ServeDir::new("/home/pi/projects/cedar/cedar_webapp/build/web"));

    let mut camera = asi_camera::ASICamera::new(
        asi_camera2::asi_camera2_sdk::ASICamera::new(0)).unwrap();
    camera.set_exposure_duration(Duration::from_millis(5)).unwrap();
    camera.set_gain(Gain::new(100)).unwrap();
    let shared_camera = Arc::new(Mutex::new(camera));

    // Build the grpc service.
    let grpc = tonic::transport::Server::builder()
        .accept_http1(true)
        .layer(GrpcWebLayer::new())
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any))
        .add_service(CedarServer::new(MyCedar::new(shared_camera.clone())))
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
