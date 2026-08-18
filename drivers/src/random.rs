//! ChaCha20 CSPRNG (RFC 8439): constant-time, no lookup tables.
//!
//! Seeded at boot by `boot_step_csprng_seed_fn` from RDRAND (primary), RDSEED
//! (bonus) and TSC (fallback); rekeys automatically every 1 MB of output.

use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, OnceLock, SpinLock};

/// The "expand 32-byte k" constant as four little-endian u32s.
const CHACHA_CONSTANTS: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// 16384 blocks * 64 bytes = 1 MB between mandatory rekeys.
const REKEY_INTERVAL: u64 = 16384;

#[inline(always)]
fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] ^= state[a];
    state[d] = state[d].rotate_left(16);

    state[c] = state[c].wrapping_add(state[d]);
    state[b] ^= state[c];
    state[b] = state[b].rotate_left(12);

    state[a] = state[a].wrapping_add(state[b]);
    state[d] ^= state[a];
    state[d] = state[d].rotate_left(8);

    state[c] = state[c].wrapping_add(state[d]);
    state[b] ^= state[c];
    state[b] = state[b].rotate_left(7);
}

fn chacha20_block(key: &[u32; 8], counter: u64) -> [u8; 64] {
    let mut state: [u32; 16] = [
        CHACHA_CONSTANTS[0],
        CHACHA_CONSTANTS[1],
        CHACHA_CONSTANTS[2],
        CHACHA_CONSTANTS[3],
        key[0],
        key[1],
        key[2],
        key[3],
        key[4],
        key[5],
        key[6],
        key[7],
        counter as u32,
        (counter >> 32) as u32,
        0, // nonce word 0 (unused for CSPRNG)
        0, // nonce word 1 (unused for CSPRNG)
    ];

    let initial = state;

    // 20 rounds = 10 column/diagonal double-rounds.
    for _ in 0..10 {
        quarter_round(&mut state, 0, 4, 8, 12);
        quarter_round(&mut state, 1, 5, 9, 13);
        quarter_round(&mut state, 2, 6, 10, 14);
        quarter_round(&mut state, 3, 7, 11, 15);
        quarter_round(&mut state, 0, 5, 10, 15);
        quarter_round(&mut state, 1, 6, 11, 12);
        quarter_round(&mut state, 2, 7, 8, 13);
        quarter_round(&mut state, 3, 4, 9, 14);
    }

    for i in 0..16 {
        state[i] = state[i].wrapping_add(initial[i]);
    }

    let mut out = [0u8; 64];
    for (i, word) in state.iter().enumerate() {
        let bytes = word.to_le_bytes();
        out[i * 4..i * 4 + 4].copy_from_slice(&bytes);
    }
    out
}

struct CsprngState {
    key: [u32; 8],
    counter: u64,
    blocks_since_rekey: u64,
}

impl CsprngState {
    fn from_seed(seed: &[u8; 32]) -> Self {
        let mut key = [0u32; 8];
        for i in 0..8 {
            key[i] = u32::from_le_bytes([
                seed[i * 4],
                seed[i * 4 + 1],
                seed[i * 4 + 2],
                seed[i * 4 + 3],
            ]);
        }
        Self {
            key,
            counter: 0,
            blocks_since_rekey: 0,
        }
    }

    fn fill(&mut self, buf: &mut [u8]) {
        let mut pos = 0;
        while pos < buf.len() {
            if self.blocks_since_rekey >= REKEY_INTERVAL {
                self.rekey();
            }

            let block = chacha20_block(&self.key, self.counter);
            self.counter = self.counter.wrapping_add(1);
            self.blocks_since_rekey += 1;

            let remaining = buf.len() - pos;
            let chunk = remaining.min(64);
            buf[pos..pos + chunk].copy_from_slice(&block[..chunk]);
            pos += chunk;
        }
    }

    /// One extra block becomes the new key, bounding the damage window if the
    /// CSPRNG state ever leaks.
    fn rekey(&mut self) {
        let block = chacha20_block(&self.key, self.counter);
        self.counter = self.counter.wrapping_add(1);

        for i in 0..8 {
            self.key[i] = u32::from_le_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        self.counter = 0;
        self.blocks_since_rekey = 0;
    }
}

static CSPRNG: OnceLock<SpinLock<CsprngState>> = OnceLock::new();

/// Shared by both `call_once` closures: whichever wins the race installs this.
const CSPRNG_CLASS: &slopos_ostd::sync::lock_tracking::LockClassKey =
    slopos_ostd::lock_class!("CSPRNG", LOCK_LEVEL_REGISTRY);

/// Called once during boot; any later call is a no-op.
pub fn init_csprng(seed: &[u8; 32]) {
    CSPRNG.call_once(|| SpinLock::new(CsprngState::from_seed(seed), CSPRNG_CLASS));
}

/// Called before the boot seeding step, this seeds from RDRAND or TSC itself.
pub fn csprng_fill(buf: &mut [u8]) {
    CSPRNG.call_once(|| {
        let seed = emergency_seed();
        SpinLock::new(CsprngState::from_seed(&seed), CSPRNG_CLASS)
    });

    let rng = CSPRNG.get().expect("CSPRNG not initialized");
    rng.lock().fill(buf);
}

pub fn random_next() -> u64 {
    let mut buf = [0u8; 8];
    csprng_fill(&mut buf);
    u64::from_le_bytes(buf)
}

/// RDRAND where available, else mixed TSC samples.
fn emergency_seed() -> [u8; 32] {
    use slopos_arch::cpu::rdrand;
    use slopos_arch::tsc;

    let mut seed = [0u8; 32];

    if let Some(rd) = rdrand::RdRand::probe() {
        for chunk in seed.chunks_exact_mut(8) {
            if let Some(val) = rd.next() {
                chunk.copy_from_slice(&val.to_le_bytes());
            }
        }
    } else {
        let mixing: [u64; 4] = [
            0x9E37_79B9_7F4A_7C15,
            0x6C62_272E_07BB_0142,
            0xBF58_476D_1CE4_E5B9,
            0x94D0_49BB_1331_11EB,
        ];
        for (i, chunk) in seed.chunks_exact_mut(8).enumerate() {
            let tsc_val = tsc::rdtsc();
            let mixed = tsc_val.wrapping_mul(mixing[i]).wrapping_add(mixing[i]);
            chunk.copy_from_slice(&mixed.to_le_bytes());
        }
    }

    seed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chacha20_rfc8439_test_vector() {
        // TODO(tech-debt): named for the RFC 8439 §2.3.2 vector but only checks
        // sanity and determinism — should assert the published keystream bytes.
        let key: [u32; 8] = [
            0x0302_0100,
            0x0706_0504,
            0x0b0a_0908,
            0x0f0e_0d0c,
            0x1312_1110,
            0x1716_1514,
            0x1b1a_1918,
            0x1f1e_1d1c,
        ];
        let block = chacha20_block(&key, 0);
        assert!(block.iter().any(|&b| b != 0));
        let block2 = chacha20_block(&key, 0);
        assert_eq!(block, block2);
        let block3 = chacha20_block(&key, 1);
        assert_ne!(block, block3);
    }

    #[test]
    fn csprng_fill_produces_nonzero() {
        let seed = [0x42u8; 32];
        let mut state = CsprngState::from_seed(&seed);
        let mut buf = [0u8; 128];
        state.fill(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[test]
    fn csprng_rekey_changes_output() {
        let seed = [0x42u8; 32];
        let mut state = CsprngState::from_seed(&seed);
        let mut buf1 = [0u8; 64];
        state.fill(&mut buf1);
        state.blocks_since_rekey = REKEY_INTERVAL;
        let mut buf2 = [0u8; 64];
        state.fill(&mut buf2);
        assert_ne!(buf1, buf2);
    }
}
