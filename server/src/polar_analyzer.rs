// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

// Module to estimate polar axis (mis)alignment.
// See http://celestialwonders.com/articles/polaralignment/MeasuringAlignmentError.html

use log::{debug};

use cedar_elements::cedar::{ErrorBoundedValue, PolarAlignAdvice};
use cedar_elements::cedar_common::CelestialCoord;
use crate::motion_estimator::MotionEstimate;

pub struct PolarAnalyzer {
    polar_align_advice: PolarAlignAdvice,
}

impl Default for PolarAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl PolarAnalyzer {
    pub fn new() -> Self {
        PolarAnalyzer{
            polar_align_advice: PolarAlignAdvice{azimuth_correction: None,
                                                 altitude_correction: None},
        }
    }

    // This function should be called when the following conditions are all met:
    // * There is a plate solution (valid boresight_pos).
    // * The date/time and observer geographic location is known (valid hour_angle,
    //   latitude).
    // When certain other conditions are met, this function updates the
    // `polar_align_advice` state.
    pub fn process_solution(&mut self, boresight_pos: &CelestialCoord, hour_angle: f64,
                            latitude: f64, motion_estimate: &Option<MotionEstimate>) {
        self.polar_align_advice.azimuth_correction = None;
        self.polar_align_advice.altitude_correction = None;
        if motion_estimate.is_none() {
            debug!("Not updating polar alignment advice: not dwelling");
            return;
        }
        let motion_estimate = motion_estimate.as_ref().unwrap();
        // `hour_angle` and `latitude` args: degrees.
        const SIDEREAL_RATE: f64 = 15.04 / 3600.0;  // Degrees per second.
        // If we're on a tracking equatorial mount that is at least roughly
        // polar-aligned, the ra_rate will be close to zero.
        if motion_estimate.ra_rate.abs() > SIDEREAL_RATE * 0.3 {
            debug!("Not updating polar alignment advice: excessive ra_rate {}arcsec/sec",
                   motion_estimate.ra_rate * 3600.0);
            return;
        }
        let dec_rate = motion_estimate.dec_rate;  // Positive is northward drift.
        let dec_rate_error = motion_estimate.dec_rate_error;
        assert!(dec_rate_error >= 0.0);

        // Degrees (plus or minus) within which the declination must be zero for
        // polar alignment to be evaluated.
        const DEC_TOLERANCE: f64 = 15.0;

        // Hours (plus or minus) around the meridian for polar alignment azimuth
        // evaluation; hours (doubled) above east or west horizon for polar alignment
        // elevation evaluation.
        const HA_TOLERANCE: f64 = 1.0;

        let dec = boresight_pos.dec;
        if dec > DEC_TOLERANCE || dec < -DEC_TOLERANCE {
            debug!("Not updating polar alignment advice: declination {}deg", dec);
            return;
        }

        // Adjust sidereal rate for declination.
        let adjusted_sidereal_rate = SIDEREAL_RATE * dec.to_radians().cos();

        // Compute the angle formed by the declination drift rate at a right angle
        // to the adjusted_sidereal_rate. Degrees.
        let mut dec_drift_angle = (dec_rate / adjusted_sidereal_rate).atan().to_degrees();
        let mut dec_drift_angle_error =
            (dec_rate_error / adjusted_sidereal_rate).atan().to_degrees();
        assert!(dec_drift_angle_error >= 0.0);

        // `hour_angle` arg is in degrees.
        let ha_hours = hour_angle / 15.0;
        if ha_hours > -HA_TOLERANCE && ha_hours < HA_TOLERANCE {
            // Near meridian. We can estimate polar alignment azimuth deviation by
            // declination drift method.

            // Adjust for deviation from optimal HA.
            let ha_correction = hour_angle.to_radians().cos();
            dec_drift_angle /= ha_correction;
            dec_drift_angle_error /= ha_correction;
            assert!(dec_drift_angle_error >= 0.0);

            // We project the azimuth_correction angle to the local horizontal.
            let latitude_correction = latitude.to_radians().cos();

            // We express polar axis azimuth correction as positive angle (clockwise
            // looking down at mount from above) or negative angle
            // (counter-clockwise), rather than in terms of east or west. This value
            // is thus independent of northern/southern hemisphere.
            let az_corr = -dec_drift_angle / latitude_correction;
            let az_corr_error = dec_drift_angle_error / latitude_correction;

            self.polar_align_advice.azimuth_correction =
                Some(ErrorBoundedValue{value: az_corr, error: az_corr_error});
            return;
        }

        // Degrees.
        let mut altitude_correction;
        if ha_hours > -6.0 && ha_hours < -6.0 + 2.0 * HA_TOLERANCE {
            // Close to rising horizon. We can estimate polar alignmwent
            // elevation deviation by declination drift method.

            // Adjust for deviation from optimal HA.
            let ha_correction = (hour_angle - -90.0).to_radians().cos();
            dec_drift_angle /= ha_correction;
            dec_drift_angle_error /= ha_correction;
            assert!(dec_drift_angle_error >= 0.0);

            // Northern hemisphere:
            // Boresight drifting south (star drifting north in FOV): polar axis too high.
            // Boresight drifting north (star drifting south in FOV): polar axis too low.
            altitude_correction = dec_drift_angle;
        } else if ha_hours < 6.0 && ha_hours > 6.0 - 2.0 * HA_TOLERANCE {
            // Close to setting horizon. We can estimate polar alignmwent
            // elevation deviation by declination drift method.

            // Adjust for deviation from optimal HA.
            let ha_correction = (hour_angle - 90.0).to_radians().cos();
            dec_drift_angle /= ha_correction;
            dec_drift_angle_error /= ha_correction;
            assert!(dec_drift_angle_error >= 0.0);

            // Northern hemisphere:
            // Boresight drifting south (star drifting north in FOV): polar axis too low.
            // Boresight drifting north (star drifting sourth in FOV): polar axis too high.
            altitude_correction = -dec_drift_angle;
        } else {
            debug!("Not updating polar alignment advice: hour angle {}h", ha_hours);
            return;
        }
        let altitude_correction_error = dec_drift_angle_error;
        if latitude < 0.0 {
            // Southern hemisphere: reverse sense of altitude guidance.
            altitude_correction = -altitude_correction;
        }
        self.polar_align_advice.altitude_correction =
            Some(ErrorBoundedValue{value: altitude_correction,
                                   error: altitude_correction_error});
    }

    pub fn get_polar_align_advice(&self) -> PolarAlignAdvice {
        self.polar_align_advice.clone()
    }
}
