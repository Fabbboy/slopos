use crate::io;

unsafe extern "C" {
    fn slopos_get_time_ms() -> u64;
    fn getpid() -> i32;
}

static mut PRNG_STATE: u64 = 0;
static mut PRNG_INITIALIZED: bool = false;

fn prng_seed() -> u64 {
    let time = unsafe { slopos_get_time_ms() };
    let pid = unsafe { getpid() } as u64;
    time.wrapping_mul(6364136223846793005)
        .wrapping_add(pid ^ 0xdeadbeef_cafebabe)
}

fn prng_next() -> u64 {
    unsafe {
        if !PRNG_INITIALIZED {
            PRNG_STATE = prng_seed();
            PRNG_INITIALIZED = true;
        }
        // xorshift64
        PRNG_STATE ^= PRNG_STATE << 13;
        PRNG_STATE ^= PRNG_STATE >> 7;
        PRNG_STATE ^= PRNG_STATE << 17;
        PRNG_STATE
    }
}

pub fn fill_bytes(bytes: &mut [u8]) {
    let mut i = 0;
    while i < bytes.len() {
        let val = prng_next();
        let chunk = val.to_ne_bytes();
        let remaining = bytes.len() - i;
        let to_copy = remaining.min(8);
        bytes[i..i + to_copy].copy_from_slice(&chunk[..to_copy]);
        i += to_copy;
    }
}
