use log::warn;
use std::time::{Duration, SystemTime};

use crate::cedar::{MotionEstimate, MotionType};
use crate::rate_estimator::RateEstimation;
use crate::tetra3_server::CelestialCoord;

#[derive(PartialEq)]
enum State {
    // MotionEstimator is newly constructed, or too much time has passed without
    // a position passed to add().
    Unknown,

    // While Unknown, a call to add() received a position. Alternately, while
    // Moving, Stopped, or SteadyRate, another add() with a very different
    // position was received.
    Moving,

    // While Moving, a call to add() received a position very similar to the
    // previous position, consistent with a fixed mount (position moving at
    // sidereal rate) or a tracking mount (position nearly motionless in ra/dec)
    // that is "motionless" (not slewing).
    Stopped,

    // From Stopped, the next add()ed position is consistent with the previous
    // two points, for either a tracking or fixed mount. We continue in SteadyRate
    // as long as newly add()ed positions are consistent with the existing rate
    // estimates.
    SteadyRate,
}

pub struct MotionEstimator {
    // The current state of this MotionEstimator.
    state: State,

    // Set whenever add() is called with position=None. If the gap persists
    // too long, `state` reverts to Unknown.
    gap: Option<SystemTime>,

    gap_tolerance: Duration,

    // Time passed to most recent add() call, if any.
    prev_time: Option<SystemTime>,
    // Position passed to most recent add() call, if any.
    prev_position: Option<CelestialCoord>,

    // Tracking rate estimation, used when add()ed positions are consistent with
    // a motionless fixed mount or tracking mount.
    ra_rate: RateEstimation,
    dec_rate: RateEstimation,
}

impl MotionEstimator {
    // `gap_tolerance` The amount of time add() calls can have position=None
    //     before our state reverts to Unknown.
    pub fn new(gap_tolerance: Duration) -> Self {
        MotionEstimator{
            state: State::Unknown,
            gap: None,
            gap_tolerance,
            prev_time: None,
            prev_position: None,
            ra_rate: RateEstimation::new(100),
            dec_rate: RateEstimation::new(100),
        }
    }

    // `time` Time at which the image corresponding to `boresight_position` was
    //     captured. Must not be earlier than `time` passed to previous add()
    //     call.
    // `position` A successfully plate-solved determination of the telescope's
    //     aim point as of `time`. Omitted if there was no solution (perhaps
    //     because the telescope is slewing).
    pub fn add(&mut self, mut time: SystemTime, position: Option<CelestialCoord>) {
        let prev_time = self.prev_time;
        let prev_pos = self.prev_position.clone();
        self.prev_time = Some(time);
        self.prev_position = position.clone();
        if prev_time.is_none() {
            // This is the very first call to add().
            if position.is_some() {
                self.state = State::Moving;
            }
            return;
        }
        if time < prev_time.unwrap() {
            warn!("Time arg regressed from {:?} to {:?}", prev_time.unwrap(), time);
            time = prev_time.unwrap() + Duration::from_micros(1);
            self.prev_time = Some(time);
        }
        if position.is_none() {
            if self.state == State::Unknown {
                return;
            }
            if let Some(gap) = self.gap {
                // Has gap persisted for too long?
                if time.duration_since(gap).unwrap() > self.gap_tolerance {
                    self.state = State::Unknown;
                    self.gap = None;
                    self.ra_rate.clear();
                    self.dec_rate.clear();
                }
            } else {
                // New gap is starting.
                self.gap = Some(time);
            }
            return;
        }
        let position = position.unwrap();
        self.gap = None;
        match self.state {
            State::Unknown => {
                self.state = State::Moving;
            },
            State::Moving => {
                // Compare new position/time to previous position/time.
                if self.is_stopped(time, &position) {
                    self.state = State::Stopped;
                    // Enter the two points into our rate trackers.
                    let prev_pos = prev_pos.as_ref().unwrap();
                    self.ra_rate.add(
                        prev_time.unwrap(), prev_pos.ra as f64);
                    self.ra_rate.add(time, position.ra as f64);
                    self.dec_rate.add(
                        prev_time.unwrap(), prev_pos.dec as f64);
                    self.dec_rate.add(time, position.dec as f64);
                }
            },
            State::Stopped => {
                // Compare new position/time to previous position/time.
                if !self.is_stopped(time, &position) {
                    self.state = State::Moving;
                    self.ra_rate.clear();
                    self.dec_rate.clear();
                } else {
                    // See if the RA rate (tracking vs non-tracking mount) is
                    // consistent with the first two points in our rate
                    // tracker.
                    assert!(self.ra_rate.count() == 2);
                    assert!(self.dec_rate.count() == 2);
                    let elapsed_secs =
                        time.duration_since(prev_time.unwrap()).unwrap().as_secs_f32();
                    let prev_pos = prev_pos.as_ref().unwrap();
                    let ra_rate =
                        Self::ra_change(prev_pos.ra, position.ra) / elapsed_secs;
                    if (ra_rate - self.ra_rate.slope() as f32).abs() <
                        Self::SIDEREAL_RATE / 4.0
                    {
                        self.state = State::SteadyRate;
                        self.ra_rate.add(time, position.ra as f64);
                        self.dec_rate.add(time, position.dec as f64);
                    } else {
                        self.state = State::Moving;
                        self.ra_rate.clear();
                        self.dec_rate.clear();
                    }
                }
            },
            State::SteadyRate => {
                if self.ra_rate.fits_trend(time, position.ra as f64, /*sigma=*/10.0) &&
                    self.dec_rate.fits_trend(time, position.dec as f64, /*sigma=*/10.0)
                {
                    self.ra_rate.add(time, position.ra as f64);
                    self.dec_rate.add(time, position.dec as f64);
                } else {
                    self.state = State::Moving;
                    self.ra_rate.clear();
                    self.dec_rate.clear();
                }
            },
        }
    }

    pub fn get_estimate(&self) -> MotionEstimate {
        match self.state {
            State::Unknown => {
                MotionEstimate{camera_motion: MotionType::Unknown.into(),
                               ..Default::default()}
            },
            State::Moving | State::Stopped => {
                MotionEstimate{camera_motion: MotionType::Moving.into(),
                               ..Default::default()}
            },
            State::SteadyRate => {
                // Dwelling. Determine whether untracked or tracked.
                if Self::close_to_sidereal_rate(self.ra_rate.slope() as f32) {
                    MotionEstimate{camera_motion: MotionType::DwellUntracked.into(),
                                   ..Default::default()}
                } else {
                    MotionEstimate{
                        camera_motion: MotionType::DwellTracked.into(),
                        ra_rate: Some(self.ra_rate.slope() as f32),
                        ra_rate_error: Some(self.ra_rate.rate_interval_bound() as f32),
                        dec_rate: Some(self.dec_rate.slope() as f32),
                        dec_rate_error: Some(self.dec_rate.rate_interval_bound() as f32),
                    }
                }
            },
        }
    }

    fn is_stopped(&self, time: SystemTime, pos: &CelestialCoord) -> bool {
        let elapsed_secs =
            time.duration_since(self.prev_time.unwrap()).unwrap().as_secs_f32();
        let prev_pos = self.prev_position.as_ref().unwrap();

        let dec_rate = Self::dec_change(prev_pos.dec, pos.dec) / elapsed_secs;
        if dec_rate.abs() > Self::SIDEREAL_RATE / 4.0 {
            return false;
        }
        let ra_rate = Self::ra_change(prev_pos.ra, pos.ra) / elapsed_secs;
        // Two cases that qualify as "stopped". Non-tracking mount: position
        // is changing at sidereal rate. Tracking mount: position is changing
        // very little.
        if Self::close_to_sidereal_rate(ra_rate) {
            return true;  // Non-tracking mount.
        }
        ra_rate.abs() < Self::SIDEREAL_RATE / 4.0  // Tracking mount?
    }

    fn close_to_sidereal_rate(rate: f32) -> bool {
        rate > 0.75 * Self::SIDEREAL_RATE && rate < 1.25 * Self::SIDEREAL_RATE
    }

    const SIDEREAL_RATE: f32 = 15.04;  // Arcseconds per second.

    // Computes the change in declination between `prev_dec` and `cur_dec`. All
    // are in degrees.
    fn dec_change(prev_dec: f32, cur_dec: f32) -> f32 {
        cur_dec - prev_dec
    }

    // Computes the change in right ascension between `prev_ra` and `cur_ra`.
    // Care is taken when crossing the 360..0 boundary.
    // All are in degrees.
    fn ra_change(mut prev_ra: f32, mut cur_ra: f32) -> f32 {
        if prev_ra < 45.0 && cur_ra > 315.0 {
            prev_ra += 360.0;
        }
        if cur_ra < 45.0 && prev_ra > 315.0 {
            cur_ra += 360.0;
        }
        cur_ra - prev_ra
    }
}

#[cfg(test)]
mod tests {
    // extern crate approx;
    // use approx::assert_abs_diff_eq;
    use super::*;

    #[test]
    fn test_dec_change() {
        assert_eq!(MotionEstimator::dec_change(10.0, 15.0), 5.0);
        assert_eq!(MotionEstimator::dec_change(-10.0, 15.0), 25.0);
        assert_eq!(MotionEstimator::dec_change(15.0, 10.0), -5.0);
    }

    #[test]
    fn test_ra_change() {
        assert_eq!(MotionEstimator::ra_change(10.0, 15.0), 5.0);
        assert_eq!(MotionEstimator::ra_change(350.0, 355.0), 5.0);
        assert_eq!(MotionEstimator::ra_change(355.0, 360.0), 5.0);
        assert_eq!(MotionEstimator::ra_change(356.0, 1.0), 5.0);

        assert_eq!(MotionEstimator::ra_change(15.0, 10.0), -5.0);
        assert_eq!(MotionEstimator::ra_change(355.0, 350.0), -5.0);
        assert_eq!(MotionEstimator::ra_change(360.0, 355.0), -5.0);
        assert_eq!(MotionEstimator::ra_change(1.0, 356.0), -5.0);
    }

}  // mod tests.
