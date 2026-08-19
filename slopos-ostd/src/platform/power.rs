//! The terminal power primitives, behind a capability witness.
//!
//! Both take a `&Cap<'_, Power>`, so a call site that never checked does not
//! compile.
//!
//! The sequence itself stays in `boot`, which owns the ACPI and UEFI state it
//! needs and sits above OSTD, so it is registered in rather than called out to.
//! OSTD owns the authority; `boot` owns the mechanism.
//!
//! Kernel-initiated shutdowns have no caller to check and mint through
//! [`kernel_authority`]. That is the one seam in the compile-error claim, held
//! closed by `scripts/check_authority_reachability.sh` rather than by types.

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
pub fn shutdown(_cap: &Cap<'_, Power>, reason: *const c_char) -> ! {
    dispatch_shutdown(reason)
}

/// Reboot the machine. See [`shutdown`].
pub fn reboot(_cap: &Cap<'_, Power>, reason: *const c_char) -> ! {
    dispatch_reboot(reason)
}

/// Mint a `Power` witness for a caller that has no credential to check — the
/// kconsole commands, the harness exit, the panic path.
///
/// Public because those register from `mm`, `sched`, `core` and `boot`. What
/// keeps the set small is the reachability gate's tracked list, not this
/// signature.
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

/// No implementation registered — before `boot` runs, or under a host test.
/// Parking is the only answer a `!` return admits.
fn halt_forever() -> ! {
    loop {
        core::hint::spin_loop();
    }
}
