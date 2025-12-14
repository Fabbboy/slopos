#![allow(dead_code)]

use core::sync::OnceLock;
use slopos_lib::tsc;
use spin::Mutex;

const DEFAULT_LFSR_SEED: u64 = 0xACE1u64;

#[derive(Clone, Copy)]
pub struct Lfsr64 {
    state: u64,
}

impl Lfsr64 {
    pub fn with_seed(seed: u64) -> Self {
        let s = if seed == 0 { DEFAULT_LFSR_SEED } else { seed };
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

static RNG: OnceLock<Mutex<Lfsr64>> = OnceLock::new();

pub fn random_next() -> u64 {
    let rng = RNG.get_or_init(|| Mutex::new(Lfsr64::from_tsc()));
    let mut guard = rng.lock();
    guard.next()
}

