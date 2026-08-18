//! Shell-side SIGINT interrupt state.
//!
//! Builtins run in the shell's own process, so a terminal-generated SIGINT
//! targets the shell: the handler only records it and builtin loops poll
//! [`take_pending`] as their cancellation point.  The shell never
//! pattern-matches Ctrl+C; termios ISIG/VINTR/NOFLSH stay authoritative.

use std::sync::atomic::{AtomicBool, Ordering};

use slopos_abi::signal::SIGINT;

use crate::syscall::process;

/// Conventional shell exit status for a SIGINT-terminated command (128 + 2).
pub const EXIT_INTERRUPTED: i32 = 130;

static INTERRUPT_STATE: AtomicBool = AtomicBool::new(false);
static IN_FORKED_CHILD: AtomicBool = AtomicBool::new(false);

extern "C" fn record_sigint(_signum: i32) {
    INTERRUPT_STATE.store(true, Ordering::Release);
}

/// Install the flag-setting SIGINT handler.
pub fn install() {
    let _ = process::set_signal_handler(SIGINT, record_sigint);
}

/// Mark a freshly forked pipeline child: it takes the default SIGINT
/// disposition, so it must not consult the shell's interrupt flag.
pub fn mark_forked_child() {
    IN_FORKED_CHILD.store(true, Ordering::Release);
}

/// True in a process forked to run one pipeline stage — a subshell, where a
/// builtin that would end the shell ends only the stage.
pub fn in_forked_child() -> bool {
    IN_FORKED_CHILD.load(Ordering::Acquire)
}

/// Cancellation point for long-running builtin loops; consumes the flag.
pub fn take_pending() -> bool {
    if IN_FORKED_CHILD.load(Ordering::Acquire) {
        return false;
    }
    INTERRUPT_STATE.swap(false, Ordering::AcqRel)
}

/// Discard any interrupt recorded outside command execution.
pub fn clear() {
    INTERRUPT_STATE.store(false, Ordering::Release);
}
