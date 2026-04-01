// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::time::{Duration, Instant};

use cedar_elements::cedar_common::CelestialCoord;
use log::debug;

use crate::rate_estimator::RateEstimation;

pub struct MotionEstimate {
    // Estimated rate of RA boresight movement eastward (positive) or westward
    // (negative). Unit is degrees per second.
    pub ra_rate: f64,
    // Estimate of the RMS error in `ra_rate`.
    pub ra_rate_error: f64,

    // Estimated rate of DEC boresight movement northward (positive) or
    // southward (negative). Unit is degrees per second.
    pub dec_rate: f64,
    // Estimate of the RMS error in `dec_rate`.
    pub dec_rate_error: f64,
}

enum State {
    // MotionEstimator is newly constructed, or too much time has passed
    // without a position passed to add().
    Unknown,

    // While Unknown, a call to add() received a position. Alternately, while
    // Moving, Stopped, or SteadyRate, another add() with a very different
    // position was received.
    Moving,

    // While Moving, a call to add() received a position very similar to the
    // previous position, consistent with a fixed mount (position moving at
    // sidereal rate) or a tracking mount (position nearly motionless in
    // ra/dec) that is motionless (i.e. tracking the sky but not slewing).
    Stopped,

    // From Stopped, the next add()ed position is consistent with the previous
    // point, for either a tracking or fixed mount. We continue in SteadyRate
    // as long as newly add()ed positions are consistent with the existing
    // rate estimates.
    SteadyRate { ra_rate: RateEstimation, dec_rate: RateEstimation },
}

impl State {
    fn is_unknown(&self) -> bool {
        matches!(self, State::Unknown)
    }
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            State::Unknown => write!(f, "Unknown"),
            State::Moving => write!(f, "Moving"),
            State::Stopped => write!(f, "Stopped"),
            State::SteadyRate { .. } => write!(f, "SteadyRate"),
        }
    }
}

pub struct MotionEstimator {
    // The current state of this MotionEstimator.
    state: State,

    // How long we tolerate lack of position updates before reverting to
    // Unknown state.
    gap_tolerance: Duration,

    // When in SteadyRate, how long we tolerate (and discard) position updates
    // not consistent with `ra_rate` and `dec_rate` before reverting to Moving
    // state.
    bump_tolerance: Duration,

    // Time/position passed to most recent add() call. Updated only for add()
    // calls with non-None position arg.
    prev_time: Option<Instant>,
    prev_position: Option<CelestialCoord>,
}

impl MotionEstimator {
    // `gap_tolerance` The amount of time add() calls can have position=None
    //     before our state reverts to Unknown.
    pub fn new(gap_tolerance: Duration, bump_tolerance: Duration) -> Self {
        MotionEstimator {
            state: State::Unknown,
            gap_tolerance,
            bump_tolerance,
            prev_time: None,
            prev_position: None,
        }
    }

    // `time` Time at which the image corresponding to `boresight_position` was
    //     captured. Must be non-decreasing across successive add() calls.
    // `position` A successfully plate-solved determination of the telescope's
    //     aim point as of `time`. None if there was no solution (perhaps
    //     because the telescope is slewing).
    // `position_rmse` If `position` is provided, this will be the RMS error (in
    //     arcseconds) of the plate solution. This represents the noise level
    //     associated with `position`.
    pub fn add(
        &mut self,
        time: &Instant,
        position: Option<CelestialCoord>,
        position_rmse: Option<f64>,
    ) {
        let prev_time = self.prev_time;
        let prev_pos = self.prev_position.clone();
        if position.is_some() {
            self.prev_time = Some(*time);
            self.prev_position = position.clone();
        }
        let Some(prev_time) = prev_time else {
            assert!(prev_pos.is_none());
            if position.is_some() {
                // This is the first call to add() with a position.
                self.set_state(State::Moving);
            }
            return;
        };

        let Some(position) = position else {
            if self.state.is_unknown() {
                return;
            }
            // Has gap persisted for too long?
            if time.duration_since(prev_time) > self.gap_tolerance {
                self.set_state(State::Unknown);
            }
            return;
        };
        let position_rmse = position_rmse.unwrap() / 3600.0; // arcsec->deg.
        let prev_pos = prev_pos.expect("prev_pos is Some when prev_time is Some");
        match self.state {
            State::Unknown => {
                self.set_state(State::Moving);
            }
            State::Moving => {
                // Compare new position/time to previous position/time.
                if Self::is_stopped(time, &position, position_rmse,
                                    prev_time, &prev_pos) {
                    self.set_state(State::Stopped);
                }
            }
            State::Stopped => {
                // Compare new position/time to previous position/time. Are we
                // still stopped? TODO: require a few add()
                // calls in Stopped before advancing to SteadyRate?
                if Self::is_stopped(time, &position, position_rmse,
                                    prev_time, &prev_pos) {
                    // Enter SteadyRate and initialize ra/dec RateEstimation
                    // objects with the current and previous positions/times.
                    let mut ra_rate = RateEstimation::new(1000, &prev_time, prev_pos.ra);
                    ra_rate.add(time, position.ra, position_rmse);
                    let mut dec_rate = RateEstimation::new(1000, &prev_time, prev_pos.dec);
                    dec_rate.add(time, position.dec, position_rmse);
                    self.set_state(State::SteadyRate { ra_rate, dec_rate });
                } else {
                    self.set_state(State::Moving);
                }
            }
            State::SteadyRate { ref mut ra_rate, ref mut dec_rate } => {
                if ra_rate.fits_trend(time, position.ra, /* sigma= */ 10.0)
                    && dec_rate.fits_trend(time, position.dec, /* sigma= */ 10.0)
                {
                    ra_rate.add(time, position.ra, position_rmse);
                    dec_rate.add(time, position.dec, position_rmse);
                } else {
                    // Has rate trend violation persisted for too long?
                    if time.duration_since(ra_rate.last_time()) > self.bump_tolerance {
                        self.set_state(State::Moving);
                    }
                }
            }
        }
    }

    fn set_state(&mut self, state: State) {
        debug!("state -> {:?}", state);
        self.state = state;
    }

    /// Returns the current MotionEstimate, if any. If the boresight is not
    /// dwelling (relatively motionless), None is returned.
    pub fn get_estimate(&self) -> Option<MotionEstimate> {
        let State::SteadyRate { ra_rate, dec_rate } = &self.state else {
            return None;
        };
        if ra_rate.count() < 3 {
            None
        } else {
            Some(MotionEstimate {
                ra_rate: ra_rate.slope(),
                ra_rate_error: ra_rate.rate_interval_bound(),
                dec_rate: dec_rate.slope(),
                dec_rate_error: dec_rate.rate_interval_bound(),
            })
        }
    }

    // pos_rmse: position error estimate in degrees.
    fn is_stopped(
        time: &Instant,
        pos: &CelestialCoord,
        pos_rmse: f64,
        prev_time: Instant,
        prev_pos: &CelestialCoord,
    ) -> bool {
        let elapsed_secs = time.duration_since(prev_time).as_secs_f64();

        // Max movement rate below which we are considered to be stopped.
        let max_rate = f64::max(pos_rmse * 8.0, Self::SIDEREAL_RATE * 2.0);

        let dec_rate = (pos.dec - prev_pos.dec) / elapsed_secs;
        if dec_rate.abs() > max_rate {
            return false;
        }
        let ra_rate = Self::ra_change(prev_pos.ra, pos.ra) / elapsed_secs;
        ra_rate.abs() <= max_rate
    }

    const SIDEREAL_RATE: f64 = 15.04 / 3600.0; // Degrees per second.

    // Computes the change in right ascension between `prev_ra` and `cur_ra`.
    // Care is taken when crossing the 360..0 boundary.
    // All are in degrees.
    fn ra_change(mut prev_ra: f64, mut cur_ra: f64) -> f64 {
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
    use super::*;

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
} // mod tests.
