// Copyright (c) 2025 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

pub mod astro_util;
pub mod cedar_sky_trait;
pub mod image_utils;
pub mod reservoir_sampler;
pub mod solver_trait;
pub mod value_stats;
pub mod wifi_trait;

pub mod cedar {
    // The string specified here must match the proto package name.
    tonic::include_proto!("cedar");
}
pub mod cedar_common {
    // The string specified here must match the proto package name.
    tonic::include_proto!("cedar_common");
}
pub mod cedar_sky {
    // The string specified here must match the proto package name.
    tonic::include_proto!("cedar_sky");
}
