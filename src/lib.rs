// Copyright (c) 2023 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

pub mod astro_util;
pub mod calibrator;
pub mod detect_engine;
pub mod motion_estimator;
pub mod polar_analyzer;
pub mod position_reporter;
pub mod rate_estimator;
pub mod reservoir_sampler;
pub mod scale_image;
pub mod solve_engine;
pub mod tetra3_subprocess;
pub mod value_stats;

pub mod tetra3_server {
    tonic::include_proto!("tetra3_server");
}
pub mod cedar {
    // The string specified here must match the proto package name.
    tonic::include_proto!("cedar");
}

