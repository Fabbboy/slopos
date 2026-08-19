//! The terminal power primitives, behind a capability witness.
//!
//! These are the only two operations in the kernel whose effect is the whole
//! machine and which cannot be undone or observed afterwards. Both take a
//! `&Cap<'_, Power>`, so a call site that never checked does not compile —
//! which turns a class of permission-check placement bug into a type error for
//! the paths this covers.
//!
//! # Why the implementation is still a function pointer
//!
//! The actual sequence — quiescing interrupt controllers, flushing
//! filesystems, walking the reboot-method table — lives in `boot`, which sits
//! *above* OSTD in the dependency order and owns the ACPI and UEFI state it
//! needs. OSTD cannot call it directly. So this module owns the *authority*
//! (the witness, and the single choke point every caller funnels through)
//! while `boot` keeps the mechanism, registered once at init.
//!
//! # The documented seam
//!
//! Kernel-initiated shutdowns — the kconsole commands, the test harness's exit
//! path — have no syscall caller and therefore no capability to check. They
//! mint through [`kernel_authority`], which is public. That is the one place
//! the compile-error claim has a seam, and it is held closed by
//! `scripts/check_authority_reachability.sh` against a tracked list of callers
//! rather than by the type system.

use core::ffi::c_char;

use crate::authority::{Cap, Power};
use crate::sync::OnceLock;

/// The machine-specific implementation, registered by `boot` at init.
pub struct PowerOps {
    pub shutdown: fn(reason: *const c_char) -> !,
    pub reboot: fn(reason: *const c_char) -> !,
}

static POWER_OPS: OnceLock<PowerOps> = OnceLock::new();

/// Install the machine-specific power sequence. Called once, from `boot`,
/// before any task runs.
///
/// A second registration is ignored rather than swapping the implementation
/// under a caller mid-shutdown.
pub fn register(ops: PowerOps) {
    let mut ops = Some(ops);
    POWER_OPS.call_once(|| ops.take().expect("call_once runs its closure once"));
}

fn ops() -> Option<&'static PowerOps> {
    POWER_OPS.get()
}

/// Power the machine off.
///
/// The witness is the whole point: `&Cap<'_, Power>` can only be produced by
/// the authority checker, so this cannot be reached without one having run.
pub fn shutdown(_cap: &Cap<'_, Power>, reason: *const c_char) -> ! {
    dispatch_shutdown(reason)
}

/// Reboot the machine. See [`shutdown`].
pub fn reboot(_cap: &Cap<'_, Power>, reason: *const c_char) -> ! {
    dispatch_reboot(reason)
}

/// Mint a `Power` witness for a kernel-initiated shutdown.
///
/// For the callers that have no syscall context and therefore no credential to
/// check: the kconsole destructive commands, the test harness's exit, the
/// panic path. Their authority comes from *being the kernel*, which no runtime
/// check can establish and no type can express.
///
/// Public by necessity — kconsole commands register from `mm`, `sched`, `core`
/// and `boot`. Held to a tracked caller list by
/// `scripts/check_authority_reachability.sh`; that gate, not this signature, is
/// what keeps the set small.
pub fn kernel_authority() -> Cap<'static, Power> {
    crate::authority::mint_kernel_power()
}

fn dispatch_shutdown(reason: *const c_char) -> ! {
    match ops() {
        Some(ops) => (ops.shutdown)(reason),
        None => halt_forever(),
    }
}

fn dispatch_reboot(reason: *const c_char) -> ! {
    match ops() {
        Some(ops) => (ops.reboot)(reason),
        None => halt_forever(),
    }
}

/// No power implementation registered — before `boot` runs, or in a unit-test
/// build. Parking is the only honest answer: returning would hand a `!` caller
/// a value it cannot have.
fn halt_forever() -> ! {
    loop {
        core::hint::spin_loop();
    }
}
