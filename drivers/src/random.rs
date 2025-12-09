#![allow(dead_code)]

use slopos_lib::tsc;

#[derive(Clone, Copy)]
pub struct Lfsr64 {
    state: u64,
}

impl Lfsr64 {
    pub fn with_seed(seed: u64) -> Self {
        let s = if seed == 0 { 0xACE1u64 } else { seed };
        Self { state: s }
    }

    pub fn from_tsc() -> Self {
        let seed = tsc::rdtsc() | 1;
        Self::with_seed(seed)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = if x == 0 { 0xfeedc0de } else { x };
        self.state
    }
}

