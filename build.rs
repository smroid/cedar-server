use prost_build;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = prost_build::Config::new();
    config.protoc_arg("--experimental_allow_proto3_optional");

    tonic_build::configure().compile_with_config(
        config,
        &["src/proto/cedar.proto", "src/proto/cedar_sky.proto",
          "src/proto/tetra3.proto"], &["src/proto"])?;
    Ok(())
}
