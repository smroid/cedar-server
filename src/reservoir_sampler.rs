// Copyright (c) 2023 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use rand::{Rng, SeedableRng};
use rand::rngs::SmallRng;

pub struct ReservoirSampler<T> {
    reservoir: Vec<T>,
    capacity: usize,
    rng: SmallRng,
    add_count: usize,
}

impl<T> ReservoirSampler<T> {
    pub fn new(capacity: usize) -> Self {
        ReservoirSampler {
            reservoir: Vec::with_capacity(capacity),
            capacity,
            rng: SmallRng::seed_from_u64(42),
            add_count: 0,
        }
    }

    // Returns:
    // bool: whether the item was added to the ReservoirSampler.
    // Option<T>: populated if an item was removed from the ReservoirSampler.
    pub fn add(&mut self, item: T) -> (bool, Option<T>) {
        self.add_count += 1;
        if self.reservoir.len() < self.capacity {
            self.reservoir.push(item);
            return (true, None);
        }
        let j = self.rng.gen_range(0..self.add_count);
        if j >= self.capacity {
            return (false, None);
        }
        // Replace: keep new sample and return the disarded sample.
        (true, Some(std::mem::replace(&mut self.reservoir[j], item)))
    }

    pub fn count(&self) -> usize {
        self.reservoir.len()
    }

    pub fn samples(&self) -> &Vec<T> {
        &self.reservoir
    }

    // Resets as if newly constructed.
    pub fn clear(&mut self) {
        self.reservoir.clear();
        self.add_count = 0;
    }
}
