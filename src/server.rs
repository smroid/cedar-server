use tonic::{transport::Server, Request, Response, Status};
use tonic_web;

use env_logger;

use crate::cedar::greeter_server::{Greeter, GreeterServer};
use crate::cedar::{HelloReply, HelloRequest};

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
        request: Request<HelloRequest>, // Accept request of type HelloRequest
    ) -> Result<Response<HelloReply>, Status> { // Return an instance of type HelloReply
        println!("Got a request: {:?}", request);

        let reply = cedar::HelloReply {
            message: format!("Hello {}!", request.into_inner().name).into(),
            // We must use .into_inner() as the fields of gRPC requests and responses are private
        };

        Ok(Response::new(reply)) // Send back our formatted greeting
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("debug")).init();

    let addr = "192.168.1.134:50051".parse()?;
    let greeter = GreeterServer::new(MyGreeter::default());

    Server::builder()
        .accept_http1(true)
        .add_service(tonic_web::enable(greeter))
        .serve(addr)
        .await?;

    Ok(())
}
