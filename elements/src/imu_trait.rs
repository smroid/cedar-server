// Copyright (c) 2025 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::time::SystemTime;

use async_trait::async_trait;
use canonical_error::CanonicalError;

// Acceleration data from IMU.
#[derive(Debug, Clone, Copy)]
pub struct AccelData {
    // m/s².
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

// Angular velocity data from IMU.
#[derive(Debug, Clone, Copy)]
pub struct GyroData {
    // Degrees/second.
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

// Describes the pointing orientation of a camera in horizon coordinates.
#[derive(Debug, Clone, Copy)]
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

// Describes the pointing orientation of a camera in equatorial coordinates.
#[derive(Debug, Clone, Copy)]
pub struct EquatorialCoordinates {
    // The position angle of the north direction in the camera field of view.
    // Angle is measured in degrees, with zero being the image's "up" direction
    // (towards y=0); a positive north roll angle means north direction is
    // counter-clockwise from image "up".
    pub north_roll_angle: f64,

    // Right ascension (degrees) of the boresight.
    pub ra: f64,

    // Declination (degrees) of the boresight.
    pub dec: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct ImuState {
    pub timestamp: SystemTime,
    pub accel: AccelData,
    pub gyro: GyroData,
}

#[async_trait]
pub trait ImuTrait {
    // For all report_xxx() functions, the timestamp must be strictly non
    // decreasing for successive calls.

    // Conveys information obtained from plate solving to the IMU fusion
    // algorithms.
    async fn report_true_camera_pointing(
        &self,
        camera_pointing: &HorizonCoordinates,
        timestamp: &SystemTime,
    );

    // No plate solution is available at the given timestamp. This is either due
    // to visual obstruction (clouds, etc) or platform motion.
    async fn report_camera_pointing_lost(&self, timestamp: &SystemTime);

    // The platform is discerned to be motionless at or before the given
    // timestamp. The caller makes this determination based on successive
    // plate solves.
    async fn report_motionless(&self, timestamp: &SystemTime);

    // The platform started moving an unknown (but small) time prior to
    // timestamp. The caller determines this based on successive plate solves
    // or on the reported jerk magnitude or on the reported angular acceleration
    // magnitude.
    async fn report_not_motionless(&self, timestamp: &SystemTime);

    // IMU-derived estimate of camera pointing as of the given time. The
    // timestamp must not precede the timestamp of the most recent
    // report_true_camera_pointing() call.
    async fn get_estimated_camera_pointing(
        &self,
        timestamp: &SystemTime,
    ) -> Result<HorizonCoordinates, CanonicalError>;

    // Returns the RMS error (in degrees) of the IMU calibration fit. Returns
    // None if not yet calibrated.
    async fn get_calibration_quality(&self) -> Option<f64>;

    // Returns the most recent IMU reading.
    async fn get_state(
        &self,
    ) -> Result<(ImuState, SystemTime), CanonicalError>;

    // Returns the jerk magnitude (m/s³) seen in the most recent samples.
    async fn get_jerk_magnitude(&self) -> Result<(f64, SystemTime), CanonicalError>;

    // Returns the angular velocity magnitude (degrees/s) seen in the most
    // recent samples. Returns failed_precondition error if there is no IMU
    // calibration.
    async fn get_angular_velocity_magnitude(
        &self,
    ) -> Result<(f64, SystemTime), CanonicalError>;

    // Returns the IMU's model.
    fn get_model(&self) -> String;
}
