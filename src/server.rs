// Adapted from
// https://github.com/tokio-rs/axum/tree/main/examples/rest-grpc-multiplex
// https://github.com/tokio-rs/axum/blob/main/examples/static-file-server

use self::multiplex_service::MultiplexService;

use std::net::SocketAddr;

use axum::Router;
use env_logger;
use log::info;
use tower_http::{services::ServeDir, cors::CorsLayer, cors::Any};
use tonic_web::GrpcWebLayer;

use crate::cedar::greeter_server::{Greeter, GreeterServer};
use crate::cedar::{HelloReply, HelloRequest};

mod multiplex_service;

pub mod cedar {
    // The string specified here must match the proto package name.
    tonic::include_proto!("cedar");
}

#[derive(Debug, Default)]
pub struct MyGreeter {}

#[tonic::async_trait]
impl Greeter for MyGreeter {
    async fn say_hello(
        &self,
        request: tonic::Request<HelloRequest>, // Accept request of type HelloRequest
    ) -> Result<tonic::Response<HelloReply>,
                tonic::Status> { // Return an instance of type HelloReply
        info!("Got a request: {:?}", request);

        let reply = cedar::HelloReply {
            message: format!("Hello {}!", request.into_inner().name),
            // We must use .into_inner() as the fields of gRPC requests and responses are private
        };

        Ok(tonic::Response::new(reply)) // Send back our formatted greeting
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
        .add_service(GreeterServer::new(MyGreeter::default()))
        .into_service();

    // Combine them into one service.
    let service = MultiplexService::new(rest, grpc);

    let addr = SocketAddr::from(([192, 168, 1, 134], 8080));
    hyper::Server::bind(&addr)
        .serve(tower::make::Shared::new(service))
        .await
        .unwrap();
}
