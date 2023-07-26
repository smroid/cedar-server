use tonic::{transport::Server, Request, Response, Status};

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
//    let addr = "[::1]:50051".parse()?;
    let addr = "127.0.0.1:8080".parse()?;
    let greeter = MyGreeter::default();

    Server::builder()
        .add_service(GreeterServer::new(greeter))
        .serve(addr)
        .await?;

    Ok(())
}
