use crate::cedar::image_client::ImageClient;
use crate::cedar::ImageRequest;

pub mod cedar {
    tonic::include_proto!("cedar");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = ImageClient::connect("http://192.168.1.134:8080").await?;

    let request = tonic::Request::new(ImageRequest {});

    let response = client.get_image(request).await?;

    println!("RESPONSE={:?}", response);

    Ok(())
}
