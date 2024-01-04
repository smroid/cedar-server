use medians::Medianf64;
use rolling_stats;
use statistical;

#[derive(Clone, Debug, Default)]
pub struct DescriptiveStats {
    pub min: f64,
    pub max: f64,

    pub mean: f64,
    pub stddev: f64,

    // Omitted for `session` in ValueStats.
    pub median: Option<f64>,
    pub median_absolute_deviation: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct ValueStats {
    // Stats from recent values.
    pub recent: DescriptiveStats,

    // Stats from ValueStatsAccumulator::new(), or since the last
    // ValueStatsAccumulator::reset_session() call.
    pub session: DescriptiveStats,
}

pub struct ValueStatsAccumulator {
    pub value_stats: ValueStats,

    // State for `recent`.
    circular_buffer: CircularBuffer,

    // State for `session`.
    rolling_stats: rolling_stats::Stats<f64>,
}

impl ValueStatsAccumulator {
    pub fn new(capacity: usize) -> Self {
        Self {
            value_stats: ValueStats {
                recent: DescriptiveStats{..Default::default()},
                session: DescriptiveStats{..Default::default()},
            },
            circular_buffer: CircularBuffer::new(capacity),
            rolling_stats: rolling_stats::Stats::<f64>::new(),
        }
    }

    pub fn add_value(&mut self, value: f64) {
        self.circular_buffer.push(value);
        self.rolling_stats.update(value);

        let recent_values = self.circular_buffer.unordered_contents();
        let recent_stats = &mut self.value_stats.recent;
        recent_stats.min =
            *recent_values.iter().min_by(|a, b| a.total_cmp(b)).unwrap();
        recent_stats.max =
            *recent_values.iter().max_by(|a, b| a.total_cmp(b)).unwrap();
        recent_stats.mean = statistical::mean(recent_values);
        if recent_values.len() > 1 {
            recent_stats.stddev = statistical::standard_deviation(
                recent_values, Some(recent_stats.mean));
        }
        recent_stats.median = Some(recent_values.medf_unchecked());
        recent_stats.median_absolute_deviation =
            Some(recent_values.madf(recent_stats.median.unwrap()));

        let session_stats = &mut self.value_stats.session;
        session_stats.min = self.rolling_stats.min;
        session_stats.max = self.rolling_stats.max;
        session_stats.mean = self.rolling_stats.mean;
        session_stats.stddev = self.rolling_stats.std_dev;
        // No median or median_absolute_deviation for session_stats.
    }

    pub fn reset_session(&mut self) {
        self.value_stats.session = DescriptiveStats{..Default::default()};
        self.rolling_stats = rolling_stats::Stats::<f64>::new();
    }
}

// We use a Vec<f64> to implement a ring buffer. We don't use VecDeque or
// similar because we want a view of all elements as a single slice, and we
// don't care about their order (VecDeque provides a slice view, but as two
// slices to represent ordering).
//
// Implementation adapted from
// https://stackoverflow.com/questions/67841977/which-rust-structure-does-this
#[derive(Debug)]
struct CircularBuffer {
    start: usize,
    data: Vec<f64>,
}

impl CircularBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            start: 0,
            data: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, item: f64) {
        if self.data.len() < self.data.capacity() {
            self.data.push(item);
        } else {
            self.data[self.start] = item;
            self.start += 1;
            self.start %= self.data.capacity();
        }
    }

    pub fn unordered_contents(&self) -> &[f64] {
        self.data.as_slice()
    }
}

#[cfg(test)]
mod tests {
    extern crate approx;
    use approx::assert_abs_diff_eq;
    use super::*;

    #[test]
    fn test_circular_buffer() {
        let mut cb = CircularBuffer::new(3);
        assert_eq!(cb.unordered_contents(), &[] as &[f64]);

        cb.push(4.0);
        assert_eq!(cb.unordered_contents(), [4.0]);

        cb.push(5.0);
        cb.push(6.0);
        assert_eq!(cb.unordered_contents(), [4.0, 5.0, 6.0]);

        cb.push(7.0);
        assert_eq!(cb.unordered_contents(), [7.0, 5.0, 6.0]);
    }

    #[test]
    fn test_value_stats_accumulator() {
        let mut vsa = ValueStatsAccumulator::new(3);

        // Empty accumulator (just constructed).
        assert_eq!(vsa.value_stats.recent.min, 0.0);
        assert_eq!(vsa.value_stats.recent.max, 0.0);
        assert_eq!(vsa.value_stats.recent.mean, 0.0);
        assert_eq!(vsa.value_stats.recent.stddev, 0.0);
        assert_eq!(vsa.value_stats.recent.median, None);
        assert_eq!(vsa.value_stats.recent.median_absolute_deviation, None);
        assert_eq!(vsa.value_stats.session.min, 0.0);
        assert_eq!(vsa.value_stats.session.max, 0.0);
        assert_eq!(vsa.value_stats.session.mean, 0.0);
        assert_eq!(vsa.value_stats.session.stddev, 0.0);
        assert_eq!(vsa.value_stats.session.median, None);
        assert_eq!(vsa.value_stats.session.median_absolute_deviation, None);

        vsa.add_value(1.5);
        vsa.add_value(3.5);
        assert_eq!(vsa.value_stats.recent.min, 1.5);
        assert_eq!(vsa.value_stats.recent.max, 3.5);
        assert_eq!(vsa.value_stats.recent.mean, 2.5);
        assert_abs_diff_eq!(vsa.value_stats.recent.stddev, 1.41, epsilon = 0.01);
        assert_eq!(vsa.value_stats.recent.median, Some(2.5));
        assert_eq!(vsa.value_stats.recent.median_absolute_deviation, Some(1.0));
        assert_eq!(vsa.value_stats.session.min, 1.5);
        assert_eq!(vsa.value_stats.session.max, 3.5);
        assert_eq!(vsa.value_stats.session.mean, 2.5);
        assert_abs_diff_eq!(vsa.value_stats.session.stddev, 1.41, epsilon = 0.01);
        assert_eq!(vsa.value_stats.session.median, None);
        assert_eq!(vsa.value_stats.session.median_absolute_deviation, None);

        // reset_session() clears session stats but not recent stats.
        vsa.reset_session();
        assert_eq!(vsa.value_stats.recent.min, 1.5);
        assert_eq!(vsa.value_stats.recent.max, 3.5);
        assert_eq!(vsa.value_stats.recent.mean, 2.5);
        assert_abs_diff_eq!(vsa.value_stats.recent.stddev, 1.41, epsilon = 0.01);
        assert_eq!(vsa.value_stats.recent.median, Some(2.5));
        assert_eq!(vsa.value_stats.recent.median_absolute_deviation, Some(1.0));
        assert_eq!(vsa.value_stats.session.min, 0.0);
        assert_eq!(vsa.value_stats.session.max, 0.0);
        assert_eq!(vsa.value_stats.session.mean, 0.0);
        assert_eq!(vsa.value_stats.session.stddev, 0.0);
        assert_eq!(vsa.value_stats.session.median, None);
        assert_eq!(vsa.value_stats.session.median_absolute_deviation, None);
    }

}  // mod tests.
