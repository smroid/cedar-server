// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

pub mod astro_util;
pub mod cedar_sky_trait;
pub mod hot_pixel_trait;
pub mod image_utils;
pub mod imu_trait;
pub mod reservoir_sampler;
pub mod solver_trait;
pub mod thread_name;
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
