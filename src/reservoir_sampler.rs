use rand::{Rng, SeedableRng};
use rand::rngs::SmallRng;

struct ReservoirSampler<T> {
    reservoir: Vec<T>,
    capacity: usize,
    rng: SmallRng,
    count: usize,
}

impl<T> ReservoirSampler<T> {
    pub fn new(capacity: usize) -> Self {
        ReservoirSampler {
            reservoir: Vec::with_capacity(capacity),
            capacity,
            rng: SmallRng::seed_from_u64(42),
            count: 0,
        }
    }

    pub fn add(&mut self, item: T) -> Option<T> {
        self.count += 1;
        if self.reservoir.len() < self.capacity {
            self.reservoir.push(item);
            return None;
        }
        let j = self.rng.gen_range(0..self.count);
        if j < self.capacity {
            return Some(std::mem::replace(&mut self.reservoir[j], item));
        }
        None
    }

    pub fn reservoir(&self) -> &Vec<T> {
        &self.reservoir
    }
}
