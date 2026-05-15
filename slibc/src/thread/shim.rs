//! Safe wrappers over `thread::*` for use from tests.

use super::join;
use super::pthread_t;

pub fn pthread_equal(a: pthread_t, b: pthread_t) -> bool {
    // SAFETY: extern reads no memory; both arguments are plain u64s.
    unsafe { join::pthread_equal(a, b) != 0 }
}
