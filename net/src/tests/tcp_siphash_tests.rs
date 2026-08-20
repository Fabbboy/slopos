//! SipHash-2-4 known-answer tests.
//!
//! Vectors are from the reference test data published with the SipHash paper
//! (Aumasson & Bernstein): key `000102…0f`, message `00 01 … (len-1)`.

use slopos_testing::{TestResult, assert_eq_test, assert_test, pass};

use crate::tcp::siphash::siphash24;

const K0: u64 = 0x0706_0504_0302_0100;
const K1: u64 = 0x0f0e_0d0c_0b0a_0908;

/// Expected output for messages of length 0..=15.
const VECTORS: [u64; 16] = [
    0x726f_db47_dd0e_0e31,
    0x74f8_39c5_93dc_67fd,
    0x0d6c_8009_d9a9_4f5a,
    0x8567_6696_d7fb_7e2d,
    0xcf27_94e0_2771_87b7,
    0x1876_5564_cd99_a68d,
    0xcbc9_466e_58fe_e3ce,
    0xab02_00f5_8b01_d137,
    0x93f5_f579_9a93_2462,
    0x9e00_82df_0ba9_e4b0,
    0x7a5d_bbc5_94dd_b9f3,
    0xf4b3_2f46_226b_ada7,
    0x751e_8fbc_860e_e5fb,
    0x14ea_5627_c084_3d90,
    0xf723_ca90_8e7a_f2ee,
    0xa129_ca61_49be_45e5,
];

pub fn test_siphash24_known_answers() -> TestResult {
    let mut msg = [0u8; 16];
    for (len, &expected) in VECTORS.iter().enumerate() {
        for (i, b) in msg.iter_mut().enumerate().take(len) {
            *b = i as u8;
        }
        let got = siphash24(K0, K1, &msg[..len]);
        assert_eq_test!(got, expected, "siphash-2-4 vector");
    }
    pass!()
}

/// A one-bit key change must not leave the output predictable from the old
/// one: this is the property the FNV chain it replaced did not have.
pub fn test_siphash24_key_sensitivity() -> TestResult {
    let msg = [0x11u8; 12];
    let a = siphash24(K0, K1, &msg);
    let b = siphash24(K0 ^ 1, K1, &msg);
    let c = siphash24(K0, K1 ^ 1, &msg);
    assert_test!(a != b, "flipping a k0 bit changes the digest");
    assert_test!(a != c, "flipping a k1 bit changes the digest");
    assert_test!(b != c, "the two flips differ from each other");
    pass!()
}

slopos_testing::stest!(name = test_siphash24_known_answers, suite = tcp_siphash);
slopos_testing::stest!(name = test_siphash24_key_sensitivity, suite = tcp_siphash);
