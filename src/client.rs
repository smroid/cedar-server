use crate::cedar::greeter_client::GreeterClient;
use crate::cedar::HelloRequest;

pub mod cedar {
    tonic::include_proto!("cedar");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = GreeterClient::connect("http://192.168.1.134:50051").await?;

    let request = tonic::Request::new(HelloRequest {
        name: "Tonic".into(),
    });

    let response = client.say_hello(request).await?;

    println!("RESPONSE={:?}", response);

    Ok(())
}
