use log::warn;
use std::time::{Duration, SystemTime};

use noisy_float::types::{R64, r64};
use noisy_float::prelude::Float;
use crate::reservoir_sampler::ReservoirSampler;

struct DataPoint {
    x: SystemTime,
    y: R64,
}

// Models a one-dimension time series (float values as a function of time)
// assuming a constant rate of change. The rate is estimated from observations,
// and an estimate of the rate's uncertainty is derived from a measurement of
// the data's noise.
pub struct RateEstimation {
    // Time of the first data point to be add()ed.
    first: Option<SystemTime>,

    // Time of most recent data point to be add()ed.
    last: Option<SystemTime>,

    // The retained subset of data points that have been add()ed.
    reservoir: ReservoirSampler<DataPoint>,

    // The linear regression's slope. This is the rate of change in y per second
    // of SystemTime (x) change.
    slope: R64,

    // The linear regression's y intercept.
    intercept: R64,

    // Estimate of RMS deviation of y values compared to the linear regression
    // trend.
    noise: R64,

    // Allows part of add() logic to be incremental.
    x_sum: R64,
    y_sum: R64,
}

impl RateEstimation {
    // `capacity` governs how many add()ed points are kept to compute the rate
    // estimation. Note that even though we retain a finite number of points,
    // the estimated `slope` continues to improve over time as the time span of
    // added values increases.
    pub fn new(capacity: usize) -> Self {
        RateEstimation {
            first: None,
            last: None,
            reservoir: ReservoirSampler::<DataPoint>::new(capacity),
            slope: r64(0.0),
            intercept: r64(0.0),
            noise: r64(0.0),
            x_sum: r64(0.0),
            y_sum: r64(0.0),
        }
    }

    // Successive calls to add() must have increasing `time` arg values.
    pub fn add(&mut self, mut time: SystemTime, value: f64) {
        if self.last.is_some() && time < self.last.unwrap() {
            warn!("Time arg regressed from {:?} to {:?}", self.last.unwrap(), time);
            time = self.last.unwrap() + Duration::from_micros(1);
        }
        if self.first.is_none() {
            self.first = Some(time);
        }
        self.last = Some(time);
        if let Some(removed) = self.reservoir.add(DataPoint{x: time, y: r64(value)}) {
            let x = removed.x.duration_since(SystemTime::UNIX_EPOCH).unwrap()
                .as_secs_f64();
            self.x_sum -= x;
            self.y_sum -= removed.y;
        }
        self.x_sum +=
            time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs_f64();
        self.y_sum += r64(value);
        let count = self.reservoir.count();
        if count < 2 {
            return;
        }
        let count_r64 = r64(count as f64);
        let x_mean = self.x_sum / count_r64;
        let y_mean = self.y_sum / count_r64;

        let mut num = r64(0.0);
        let mut den = r64(0.0);
        for sample in self.reservoir.samples() {
            let x = r64(sample.x.duration_since(SystemTime::UNIX_EPOCH).unwrap()
                        .as_secs_f64());
            num += (x - x_mean) * (sample.y - y_mean);
            den += (x - x_mean) * (x - x_mean);
        }
        self.slope = num / den;
        let first_x =
            r64(self.first.as_ref().unwrap().duration_since(SystemTime::UNIX_EPOCH).unwrap()
                .as_secs_f64());
        self.intercept = y_mean - self.slope * (x_mean - first_x);

        let mut y_variance = r64(0.0);
        for sample in self.reservoir.samples() {
            let y_reg = self.estimate_value(sample.x);
            y_variance += (sample.y - y_reg) * (sample.y - y_reg);
        }
        self.noise = (y_variance / count_r64).sqrt();
    }

    pub fn count(&self) -> usize {
        self.reservoir.count()
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
        let deviation = r64(value - regression_estimate).abs();
        deviation < r64(sigma) * self.noise
    }

    fn estimate_value(&self, time: SystemTime) -> f64 {
        let x = r64(time.duration_since(self.first.unwrap()).unwrap().as_secs_f64());
        (self.intercept + x * self.slope).into()
    }

    // Returns estimated rate of change in value per second of time.
    // count() must be at least 2.
    pub fn slope(&self) -> f64 {
        assert!(self.count() > 1);
        self.slope.into()
    }

    // Given the measured noise, and the range of SystemTime values contributing
    // to the model, this bound is an estimate of the +/- range of slope()
    // within which the true rate is likely to be.
    // count() must be at least 3.
    pub fn rate_interval_bound(&self) -> f64 {
        assert!(self.count() > 2);
        let time_span_secs =
            self.last.unwrap().duration_since(self.first.unwrap()).unwrap().as_secs_f64();
        (self.noise / time_span_secs).into()
    }

    // Resets as if newly constructed.
    pub fn clear(&mut self) {
        self.first = None;
        self.last = None;
        self.reservoir.clear();
        self.slope = r64(0.0);
        self.intercept = r64(0.0);
        self.noise = r64(0.0);
        self.x_sum = r64(0.0);
        self.y_sum = r64(0.0);
    }
}

#[cfg(test)]
mod tests {
    extern crate approx;
    use approx::assert_abs_diff_eq;
    use super::*;

    #[test]
    fn test_rate_estimation() {
        let mut re = RateEstimation::new(5);
        assert_eq!(re.count(), 0);

        let mut time = SystemTime::now();
        assert!(re.fits_trend(time, 1.0, /*sigma=*/5.0));
        re.add(time, 1.0);
        assert_eq!(re.count(), 1);

        // Add a second point, one second later and 0.1 higher.
        time += Duration::from_secs(1);
        assert!(re.fits_trend(time, 1.1, /*sigma=*/5.0));
        re.add(time, 1.1);
        assert_eq!(re.count(), 2);
        assert_abs_diff_eq!(re.slope(), 0.1, epsilon = 0.001);

        // Add a third point, slightly displaced from the trend.
        time += Duration::from_secs(1);
        assert!(re.fits_trend(time, 1.22, /*sigma=*/5.0));
        re.add(time, 1.22);
        assert_eq!(re.count(), 3);
        assert_abs_diff_eq!(re.slope(), 0.11, epsilon = 0.001);
        assert_abs_diff_eq!(re.rate_interval_bound(), 0.0023, epsilon = 0.0001);

        // Fourth point.
        time += Duration::from_secs(1);
        assert!(!re.fits_trend(time, 1.3, /*sigma=*/5.0));
        assert!(re.fits_trend(time, 1.31, /*sigma=*/5.0));
    }

}  // mod tests.
