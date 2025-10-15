// Copyright (c) 2025 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::time::SystemTime;

use canonical_error::CanonicalError;

// Describes the pointing orientation of a camera.
pub struct HorizonCoordinates {
    // The position angle of the zenith direction in the camera field of view.
    // Angle is measured in degrees, with zero being the image's "up" direction
    // (towards y=0); a positive zenith roll angle means the zenith is
    // counter-clockwise from image "up".
    pub zenith_roll_angle: f64,

    // Altitude (degrees, relative to the local horizon) of the boresight.
    pub altitude: f64,

    // Azimuth (degrees, positive clockwise from north) of the boresight.
    pub azimuth: f64,
}

pub struct TimeInterval {
    pub begin_time: SystemTime,
    pub end_time: SystemTime,
}

pub trait ImuTrait {
    // Conveys information obtained from plate solving to the IMU fusion
    // algorithms.
    fn report_true_camera_pointing(
        &self,
        camera_pointing: &HorizonCoordinates,
        timestamp: &SystemTime,
    );

    // No plate solution is available at the given timestamp. This is either due
    // to visual obstruction (clouds, etc) or platform motion.
    fn report_camera_pointing_lost(&self, timestamp: &SystemTime);

    // The platform is known to be motionless at or before the given timestamp.
    fn report_motionless(&self, timestamp: &SystemTime);

    // The platform started moving an unknown (but small) time prior to
    // timestamp.
    fn report_moving(&self, timestamp: &SystemTime);

    // IMU-derived estimate of current camera pointing.
    fn get_estimated_camera_pointing(
        &self,
        timestamp: &SystemTime,
    ) -> Result<HorizonCoordinates, CanonicalError>;

    // Returns the maximum jerk magnitude (m/s³) seen during the given time
    // interval.
    fn get_max_jerk_magnitude(
        &self,
        interval: &TimeInterval,
    ) -> Result<f64, CanonicalError>;
}
