//! Shell-side SIGINT interrupt state — the bash `interrupt_state`/`QUIT`
//! pattern.
//!
//! An interactive POSIX shell runs builtins in its own process, so a
//! terminal-generated SIGINT targets the shell itself.  The shell installs a
//! handler that does nothing but record the interrupt, and long-running
//! builtin loops poll [`take_pending`] as their cancellation point, aborting
//! with [`EXIT_INTERRUPTED`].
//!
//! SIGINT arrives asynchronously: the parent terminal emulator always feeds
//! keystrokes into the PTY master, so the slave line discipline performs
//! VINTR/ISIG processing and raises SIGINT against the foreground process
//! group whenever Ctrl+C is pressed — even while an in-process builtin is
//! looping.  The shell never pattern-matches Ctrl+C itself, and termios
//! settings (ISIG, VINTR, NOFLSH) stay authoritative.

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

/// Install the flag-setting SIGINT handler.  Replaces the previous
/// process-wide SIG_IGN, which made in-process builtins uninterruptible.
pub fn install() {
    let _ = process::set_signal_handler(SIGINT, record_sigint);
}

/// Mark a freshly forked pipeline child.  Forked children take the default
/// SIGINT disposition, so they must not consult the shell's interrupt flag.
pub fn mark_forked_child() {
    IN_FORKED_CHILD.store(true, Ordering::Release);
}

/// True in a process forked to run one pipeline stage.
///
/// Such a process is a subshell: a builtin that would end the shell ends only
/// the stage, and one that would consult shell-wide state must not.
pub fn in_forked_child() -> bool {
    IN_FORKED_CHILD.load(Ordering::Acquire)
}

/// Cancellation point for long-running builtin loops.  Reports and consumes
/// the interrupt flag set asynchronously by [`record_sigint`].
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
