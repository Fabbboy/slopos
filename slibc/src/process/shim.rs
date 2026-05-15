//! Safe wrappers over `process::*` for use from tests.

/// Terminate the process with `code`. Wraps `process::exit`, which is
/// `unsafe extern "C"` only because it's exported as the C `exit`
/// symbol — calling it with a plain integer is sound.
pub fn exit(code: i32) -> ! {
    // SAFETY: `process::exit` never reads memory through its argument
    // and never returns.
    unsafe { super::exit(code) }
}
