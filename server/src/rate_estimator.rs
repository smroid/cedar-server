// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use log::warn;
use std::time::{Duration, SystemTime};

use cedar_elements::reservoir_sampler::ReservoirSampler;

struct DataPoint {
    x: SystemTime,
    y: f64,
}

// Models a one-dimension time series (float values as a function of time)
// assuming a constant rate of change. The rate is estimated from observations,
// and an estimate of the rate's uncertainty is derived from a measurement of
// the data's noise.
pub struct RateEstimation {
    // Time of the first data point to be add()ed.
    first: SystemTime,

    // Time of most recent data point to be add()ed.
    last: SystemTime,

    // The retained subset of data points that have been add()ed.
    reservoir: ReservoirSampler<DataPoint>,

    // The linear regression's slope. This is the rate of change in y per second
    // of SystemTime (x) change.
    slope: f64,

    // The linear regression's y intercept.
    intercept: f64,

    // Estimate of RMS deviation of y values compared to the linear regression
    // trend.
    y_noise: f64,

    // Estimate of the standard error of the slope value.
    slope_noise: f64,

    // Allows part of add() logic to be incremental.
    x_sum: f64,
    y_sum: f64,
}

impl RateEstimation {
    // Creates a new RateEstimation and add()s the first observation to it.
    // `capacity` governs how many add()ed points are kept to compute the rate
    // estimation. Note that even though we retain a finite number of points,
    // the estimated `slope` continues to improve over time as the time span of
    // added values increases.
    pub fn new(capacity: usize, time: SystemTime, value: f64) -> Self {
        let mut re = RateEstimation {
            first: time,
            last: SystemTime::UNIX_EPOCH,
            reservoir: ReservoirSampler::<DataPoint>::new(capacity),
            slope: 0.0,
            intercept: 0.0,
            y_noise: 0.0,
            slope_noise: 0.0,
            x_sum: 0.0,
            y_sum: 0.0,
        };
        re.add(time, value, 0.0);
        re
    }

    // Successive calls to add() must have increasing `time` arg values.
    pub fn add(&mut self, time: SystemTime, value: f64, noise_estimate: f64) {
        if time <= self.last {
            // This can happen when the client updates the server's system time.
            if time <= self.last - Duration::from_secs(10) {
                warn!("Time arg regressed from {:?} to {:?}", self.last, time);
            }
            self.last = time;
            return;
        }
        self.last = time;
        let (added, removed) = self.reservoir.add(DataPoint{x: time, y: value});
        if let Some(removed) = removed {
            let x = removed.x.duration_since(SystemTime::UNIX_EPOCH).unwrap()
                .as_secs_f64();
            self.x_sum -= x;
            self.y_sum -= removed.y;
        }
        if added {
            self.x_sum +=
                time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs_f64();
            self.y_sum += value;
        }
        let count = self.reservoir.count();
        if count < 2 {
            return;
        }
        let count = count as f64;
        let x_mean = self.x_sum / count;
        let y_mean = self.y_sum / count;

        let mut num = 0.0_f64;
        let mut den = 0.0_f64;
        for sample in self.reservoir.samples() {
            let x = sample.x.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs_f64();
            num += (x - x_mean) * (sample.y - y_mean);
            den += (x - x_mean) * (x - x_mean);
        }
        // `den` will be non-zero because we require the `time` arg to be non-stationary.
        assert!(den > 0.0);
        self.slope = num / den;
        let first_x =
            self.first.duration_since(SystemTime::UNIX_EPOCH).unwrap()
                .as_secs_f64();
        self.intercept = y_mean - self.slope * (x_mean - first_x);

        let mut y_variance = 0.0_f64;
        for sample in self.reservoir.samples() {
            let y_reg = self.estimate_value(sample.x);
            y_variance += (sample.y - y_reg) * (sample.y - y_reg);
        }
        let adjusted_y_variance = f64::max(y_variance, noise_estimate * noise_estimate);
        self.y_noise = (adjusted_y_variance / count).sqrt();
        self.slope_noise = ((1.0 / (count - 2.0)) * adjusted_y_variance / den).sqrt();
    }

    pub fn count(&self) -> usize {
        self.reservoir.count()
    }

    // Returns the `time` of the most recent `add()` call.
    pub fn last_time(&self) -> SystemTime {
        self.last
    }

    // Determines if the given data point is on-trend, within `sigma` multiple of
    // the model's noise.
    // `time` must not be earlier than the first add()ed data point.
    // If count() is less than 3, returns true.
    pub fn fits_trend(&self, time: SystemTime, value: f64, sigma: f64) -> bool {
        if self.count() < 3 {
            return true;
        }
        let regression_estimate = self.estimate_value(time);
        let deviation = (value - regression_estimate).abs();
        deviation < sigma * self.y_noise
    }

    fn estimate_value(&self, time: SystemTime) -> f64 {
        let x = time.duration_since(self.first).unwrap().as_secs_f64();
        (self.intercept + x * self.slope).into()
    }

    // Returns estimated rate of change in value per second of time.
    // count() must be at least 2.
    pub fn slope(&self) -> f64 {
        assert!(self.count() > 1);
        self.slope.into()
    }

    // This bound is an estimate of the +/- range of slope() within which the
    // true rate is likely to be.
    pub fn rate_interval_bound(&self) -> f64 {
        assert!(self.count() > 2);
        self.slope_noise
    }
}

#[cfg(test)]
mod tests {
    extern crate approx;
    use approx::assert_abs_diff_eq;
    use super::*;

    #[test]
    fn test_rate_estimation() {
        let mut time = SystemTime::now();
        // Create with first point.
        let mut re = RateEstimation::new(5, time, 1.0);
        assert_eq!(re.count(), 1);

        // Add a second point, one second later and 0.1 higher.
        time += Duration::from_secs(1);
        assert!(re.fits_trend(time, 1.1, /*sigma=*/1.0));
        re.add(time, 1.1, 0.1);
        assert_eq!(re.count(), 2);
        assert_abs_diff_eq!(re.slope(), 0.1, epsilon = 0.001);

        // Add a third point, slightly displaced from the trend.
        time += Duration::from_secs(1);
        assert!(re.fits_trend(time, 1.22, /*sigma=*/1.0));
        re.add(time, 1.22, 0.1);
        assert_eq!(re.count(), 3);
        assert_abs_diff_eq!(re.slope(), 0.11, epsilon = 0.001);
        assert_abs_diff_eq!(re.rate_interval_bound(), 0.07, epsilon = 0.01);

        // Fourth point.
        time += Duration::from_secs(1);
        assert!(!re.fits_trend(time, 1.25, /*sigma=*/1.0));
        assert!(re.fits_trend(time, 1.31, /*sigma=*/1.0));
    }

}  // mod tests.
