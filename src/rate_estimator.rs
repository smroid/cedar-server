use log::warn;
use std::time::{Duration, SystemTime};

use crate::reservoir_sampler::ReservoirSampler;

struct DataPoint {
    x: SystemTime,
    y: f64,
}

// Models a one-dimension time series (float values as a function of time)
// assuming a constant rate of change. The rate is estimated from observations,
// and an estimate of the rate's uncertainty is derived from a measurement of
// the data's noise.
pub struct RateEstimation {
    // The first data point to be add()ed.
    first: Option<DataPoint>,

    // Time of most recent data point to be add()ed.
    last: Option<SystemTime>,

    // The retained subset of data points that have been add()ed after the first.
    reservoir: ReservoirSampler<DataPoint>,

    // The linear regression's slope. This is the rate of change in y per second
    // of SystemTime (x) change.
    slope: f64,

    // The linear regression's y intercept.
    intercept: f64,

    // Estimate of RMS deviation of y values compared to the linear regression
    // trend.
    noise: f64,

    // Allows part of add() logic to be incremental.
    x_sum: f64,
    y_sum: f64,
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
            slope: 0.0,
            intercept: 0.0,
            noise: 0.0,
            x_sum: 0.0,
            y_sum: 0.0,
        }
    }

    // Successive calls to add() must have increasing `time` arg values.
    pub fn add(&mut self, mut time: SystemTime, value: f64) {
        if self.last.is_some() && time < self.last.unwrap() {
            warn!("Time arg regressed from {:?} to {:?}", self.last.unwrap(), time);
            time = self.last.unwrap() + Duration::from_micros(1);
        }
        self.last = Some(time);
        if self.first.is_none() {
            self.first = Some(DataPoint{x: time, y: value});
        } else if let Some(removed) = self.reservoir.add(DataPoint{x: time, y: value}) {
            let x = removed.x.duration_since(SystemTime::UNIX_EPOCH).unwrap()
                .as_secs_f64();
            self.x_sum -= x;
            self.y_sum -= removed.y;
        }
        self.x_sum +=
            time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs_f64();
        self.y_sum += value;
        let count = 1 + self.reservoir.count();
        if count < 2 {
            return;
        }
        let x_mean = self.x_sum / count as f64;
        let y_mean = self.y_sum / count as f64;

        let first = self.first.as_ref().unwrap();
        let first_x = first.x.duration_since(SystemTime::UNIX_EPOCH).unwrap()
            .as_secs_f64();
        let mut num: f64 = (first_x - x_mean) * (first.y - y_mean);
        let mut den: f64 = (first_x - x_mean) * (first_x - x_mean);
        for sample in self.reservoir.samples() {
            let x = sample.x.duration_since(SystemTime::UNIX_EPOCH).unwrap()
                .as_secs_f64();
            num += (x - x_mean) * (sample.y - y_mean);
            den += (x - x_mean) * (x - x_mean);
        }
        self.slope = num / den;
        self.intercept = y_mean - self.slope * (x_mean - first_x);

        let y_reg = self.estimate_value(first.x);
        let mut y_variance: f64 = (first.y - y_reg) * (first.y - y_reg);
        for sample in self.reservoir.samples() {
            let y_reg = self.estimate_value(sample.x);
            y_variance += (sample.y - y_reg) * (sample.y - y_reg);
        }
        self.noise = (y_variance / count as f64).sqrt();
    }

    pub fn count(&self) -> usize {
        if self.first.is_none() {
            return 0;
        }
        1 + self.reservoir.count()
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
        deviation < sigma * self.noise
    }

    fn estimate_value(&self, time: SystemTime) -> f64 {
        let x = time.duration_since(self.first.as_ref().unwrap().x).unwrap()
            .as_secs_f64();
        self.intercept + x * self.slope
    }

    // Returns estimated rate of change in value per second of time.
    // count() must be at least 2.
    pub fn slope(&self) -> f64 {
        assert!(self.reservoir.count() > 0);
        self.slope
    }

    // Given the measured noise, and the range of SystemTime values contributing
    // to the model, this bound is an estimate of the +/- range of slope()
    // within which the true rate is likely to be.
    // count() must be at least 3.
    pub fn rate_interval_bound(&self) -> f64 {
        assert!(self.reservoir.count() > 1);
        let time_span_secs =
            self.last.unwrap().duration_since(self.first.as_ref().unwrap().x).unwrap()
            .as_secs_f64();
        self.noise / time_span_secs
    }

    // Resets as if newly constructed.
    pub fn clear(&mut self) {
        self.first = None;
        self.last = None;
        self.reservoir.clear();
        self.slope = 0.0;
        self.intercept = 0.0;
        self.noise = 0.0;
        self.x_sum = 0.0;
        self.y_sum = 0.0;
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
