//! SipHash-2-4, the keyed PRF RFC 6528 §3 calls for in an ISN generator.
//!
//! Written from the published algorithm (Aumasson & Bernstein, "SipHash: a
//! fast short-input PRF"), not derived from any implementation.

const C0: u64 = 0x736f_6d65_7073_6575;
const C1: u64 = 0x646f_7261_6e64_6f6d;
const C2: u64 = 0x6c79_6765_6e65_7261;
const C3: u64 = 0x7465_6462_7974_6573;

#[derive(Clone, Copy)]
pub struct SipHasher {
    v0: u64,
    v1: u64,
    v2: u64,
    v3: u64,
}

#[inline(always)]
fn round(v0: &mut u64, v1: &mut u64, v2: &mut u64, v3: &mut u64) {
    *v0 = v0.wrapping_add(*v1);
    *v1 = v1.rotate_left(13);
    *v1 ^= *v0;
    *v0 = v0.rotate_left(32);

    *v2 = v2.wrapping_add(*v3);
    *v3 = v3.rotate_left(16);
    *v3 ^= *v2;

    *v0 = v0.wrapping_add(*v3);
    *v3 = v3.rotate_left(21);
    *v3 ^= *v0;

    *v2 = v2.wrapping_add(*v1);
    *v1 = v1.rotate_left(17);
    *v1 ^= *v2;
    *v2 = v2.rotate_left(32);
}

impl SipHasher {
    pub fn new(k0: u64, k1: u64) -> Self {
        Self {
            v0: C0 ^ k0,
            v1: C1 ^ k1,
            v2: C2 ^ k0,
            v3: C3 ^ k1,
        }
    }

    /// Hash `msg` under the key, two compression rounds per block and four
    /// finalisation rounds.
    pub fn hash(mut self, msg: &[u8]) -> u64 {
        let len = msg.len();
        let mut chunks = msg.chunks_exact(8);

        for chunk in chunks.by_ref() {
            let m = u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ]);
            self.v3 ^= m;
            round(&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3);
            round(&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3);
            self.v0 ^= m;
        }

        let rem = chunks.remainder();
        let mut last = ((len as u64) & 0xff) << 56;
        for (i, &b) in rem.iter().enumerate() {
            last |= (b as u64) << (8 * i);
        }

        self.v3 ^= last;
        round(&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3);
        round(&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3);
        self.v0 ^= last;

        self.v2 ^= 0xff;
        round(&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3);
        round(&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3);
        round(&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3);
        round(&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3);

        self.v0 ^ self.v1 ^ self.v2 ^ self.v3
    }
}

/// One-shot keyed hash.
#[inline]
pub fn siphash24(k0: u64, k1: u64, msg: &[u8]) -> u64 {
    SipHasher::new(k0, k1).hash(msg)
}
