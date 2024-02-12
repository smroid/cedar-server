pub mod astro_util;
pub mod calibrator;
pub mod detect_engine;
pub mod position_reporter;
pub mod tetra3_subprocess;
pub mod solve_engine;
pub mod value_stats;
pub mod scale_image;

pub mod tetra3_server {
    tonic::include_proto!("tetra3_server");
}
pub mod cedar {
    // The string specified here must match the proto package name.
    tonic::include_proto!("cedar");
}

