// Copyright (c) 2025 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::time::SystemTime;

use async_trait::async_trait;
use canonical_error::CanonicalError;

// Acceleration data from IMU.
#[derive(Debug, Clone, Copy, Default)]
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

// State for IMU tracker logic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackerState {
    Motionless,
    Moving,
    Lost,
}

// Gives the current estimate of the gyro zero bias. This is the rotational
// velocity reported for each axis while the gyro is at rest.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZeroBias {
    // Degrees/sec.
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Debug, Clone)]
pub struct TransformCalibration {
    // Gives the fit quality of the current estimate of the camera-to-gyro
    // rotation transform.
    // This is the RMS residual angle distance divided by the angle distance
    // between the points from which the transform estimate was formed. If this
    // value is 0.05 (a decent fit), then during a slew of 100 degrees the
    // expected error in the IMU estimate will be 0.05 * 100 degrees or 5
    // degrees.
    pub transform_error_fraction: f64,

    // Identifies which gyro axis is parallel to the camera view axis, and the
    // degree of misalignment.
    pub camera_view_gyro_axis: String, // +X, -X, +Y, -Y, +Z, -Z.
    pub camera_view_misalignment: f64, // degrees.

    // Identifies which gyro axis is parallel to the camera up direction, and
    // the degree of misalignment.
    pub camera_up_gyro_axis: String, // +X, -X, +Y, -Y, +Z, -Z.
    pub camera_up_misalignment: f64, // degrees.
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
    // to visual obstruction (clouds, etc) or platform motion preventing star
    // detection.
    async fn report_camera_pointing_lost(&self, timestamp: &SystemTime);

    // Force get_estimated_camera_pointing() to return an error until
    // report_true_camera_pointing() is called again.
    // TODO: also discard calibration state?
    async fn reset(&self);

    // IMU-derived estimate of camera pointing as of the given time. The
    // timestamp must not precede the timestamp of the most recent
    // report_true_camera_pointing() call.
    async fn get_estimated_camera_pointing(
        &self,
        timestamp: &SystemTime,
    ) -> Result<HorizonCoordinates, CanonicalError>;

    // Returns the current state of the IMU tracker logic.
    async fn get_tracker_state(&self) -> TrackerState;

    // Returns the current calibration state, if any, of the IMU tracker.
    async fn get_calibration(
        &self,
    ) -> (Option<ZeroBias>, Option<TransformCalibration>);

    // Returns the most recent raw IMU reading.
    async fn get_state(&self)
        -> Result<(ImuState, SystemTime), CanonicalError>;

    // Returns the jerk magnitude (m/s³) seen in the most recent samples.
    async fn get_jerk_magnitude(
        &self,
    ) -> Result<(f64, SystemTime), CanonicalError>;

    // Returns the bias-adjusted angular velocity magnitude (degrees/s) seen in
    // the most recent samples. Returns failed_precondition error if there is no
    // IMU calibration.
    async fn get_angular_velocity_magnitude(
        &self,
    ) -> Result<(f64, SystemTime), CanonicalError>;

    // Returns the IMU's model.
    fn get_model(&self) -> String;
}
