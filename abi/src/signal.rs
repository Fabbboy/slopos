//! POSIX signal ABI definitions shared between kernel and userland.
//!
//! This module defines signal numbers, signal set operations, sigaction
//! structures, and related constants for the SlopOS signal subsystem.

/// Maximum number of signals. Signals are numbered 1..NSIG (signal 0 is reserved
/// for error checking in kill()).
pub const NSIG: usize = 32;

// =============================================================================
// Standard signal numbers (POSIX + Linux-compatible subset)
// =============================================================================

pub const SIGHUP: u8 = 1;
pub const SIGINT: u8 = 2;
pub const SIGQUIT: u8 = 3;
pub const SIGILL: u8 = 4;
pub const SIGTRAP: u8 = 5;
pub const SIGABRT: u8 = 6;
pub const SIGBUS: u8 = 7;
pub const SIGFPE: u8 = 8;
pub const SIGKILL: u8 = 9;
pub const SIGUSR1: u8 = 10;
pub const SIGSEGV: u8 = 11;
pub const SIGUSR2: u8 = 12;
pub const SIGPIPE: u8 = 13;
pub const SIGALRM: u8 = 14;
pub const SIGTERM: u8 = 15;
// 16 is unused
pub const SIGCHLD: u8 = 17;
pub const SIGCONT: u8 = 18;
pub const SIGSTOP: u8 = 19;
pub const SIGTSTP: u8 = 20;
pub const SIGTTIN: u8 = 21;
pub const SIGTTOU: u8 = 22;
pub const SIGWINCH: u8 = 28;

// =============================================================================
// Signal set — bitmask of up to 32 signals
// =============================================================================

/// Bitmask representing a set of signals. Bit N corresponds to signal N+1.
/// (Signal 0 does not exist; bit 0 = signal 1 = SIGHUP.)
pub type SigSet = u64;

/// Empty signal set (no signals).
pub const SIG_EMPTY: SigSet = 0;

/// One drained signal, returned by `read()` on a `FileKind::Signalfd`
/// (the SlopOS analogue of Linux `struct signalfd_siginfo`, trimmed to the
/// fields SlopOS tracks). 16 bytes, `#[repr(C)]`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SignalfdSiginfo {
    /// The signal number (1-based) that was drained.
    pub ssi_signo: u32,
    /// Signal-specific code (0 — SlopOS does not track si_code yet).
    pub ssi_code: i32,
    /// Sending task id, when known (0 otherwise).
    pub ssi_pid: u32,
    pub _pad: u32,
}

impl SignalfdSiginfo {
    pub const SERIALIZED_LEN: usize = 16;

    /// Fixed-width little-endian byte image for the `read()` copy-out.
    pub fn to_bytes(&self) -> [u8; Self::SERIALIZED_LEN] {
        let mut b = [0u8; Self::SERIALIZED_LEN];
        b[0..4].copy_from_slice(&self.ssi_signo.to_le_bytes());
        b[4..8].copy_from_slice(&self.ssi_code.to_le_bytes());
        b[8..12].copy_from_slice(&self.ssi_pid.to_le_bytes());
        b
    }
}

const _: () = assert!(core::mem::size_of::<SignalfdSiginfo>() == SignalfdSiginfo::SERIALIZED_LEN);

/// Convert a signal number (1-based) to its bitmask.
#[inline]
pub const fn sig_bit(signum: u8) -> SigSet {
    if signum == 0 || signum as usize > NSIG {
        0
    } else {
        1u64 << (signum - 1)
    }
}

/// Signals that cannot be caught, blocked, or ignored.
pub const SIG_UNCATCHABLE: SigSet = sig_bit(SIGKILL) | sig_bit(SIGSTOP);

// =============================================================================
// Signal actions
// =============================================================================

/// Special handler values for sigaction.
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

/// Flags for sa_flags in sigaction.
pub const SA_RESTORER: u64 = 0x04000000;
pub const SA_SIGINFO: u64 = 0x00000004;
pub const SA_NODEFER: u64 = 0x40000000;
pub const SA_RESETHAND: u64 = 0x80000000;
pub const SA_RESTART: u64 = 0x10000000;

/// User-visible sigaction structure passed via rt_sigaction syscall.
///
/// Layout matches the Linux kernel `struct sigaction` for x86_64.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserSigaction {
    /// Signal handler function pointer, or SIG_DFL / SIG_IGN.
    pub sa_handler: u64,
    /// Flags (SA_RESTORER, SA_SIGINFO, etc.)
    pub sa_flags: u64,
    /// Restorer function pointer (called after handler returns via SA_RESTORER).
    pub sa_restorer: u64,
    /// Signal mask to apply while handler is executing.
    pub sa_mask: SigSet,
}

impl UserSigaction {
    pub const fn default() -> Self {
        Self {
            sa_handler: SIG_DFL,
            sa_flags: 0,
            sa_restorer: 0,
            sa_mask: SIG_EMPTY,
        }
    }
}

// =============================================================================
// rt_sigprocmask how parameter
// =============================================================================

pub const SIG_BLOCK: u32 = 0;
pub const SIG_UNBLOCK: u32 = 1;
pub const SIG_SETMASK: u32 = 2;

// =============================================================================
// Default signal dispositions
// =============================================================================

/// Default action for each signal.
#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum SigDefault {
    /// Terminate the process.
    Terminate = 0,
    /// Ignore the signal.
    Ignore = 1,
    /// Stop the process (not yet implemented, treated as ignore).
    Stop = 2,
    /// Continue the process (not yet implemented, treated as ignore).
    Continue = 3,
}

/// Return the default disposition for a signal number.
///
/// Follows the POSIX default-action table for the signals SlopOS
/// defines: informational signals (`SIGCHLD`, `SIGWINCH`) are ignored
/// by default, job-control stops map to `Stop`, `SIGCONT` maps to
/// `Continue`, and everything else — including any future/unknown
/// signal number — terminates the process.
pub const fn sig_default_action(signum: u8) -> SigDefault {
    match signum {
        SIGCHLD | SIGWINCH => SigDefault::Ignore,
        SIGCONT => SigDefault::Continue,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => SigDefault::Stop,
        _ => SigDefault::Terminate,
    }
}

/// True when `signum`'s default disposition discards the signal
/// outright (`Ignore`). Used by the send-time drop check: a signal
/// that is neither blocked nor handled and whose default is `Ignore`
/// is dropped at the raise site rather than left pending, so it never
/// spuriously wakes a blocked task.
///
/// Deliberately excludes `Stop` / `Continue`: those are delivered (and
/// currently no-op at the delivery point), so implementing real job
/// control later does not require revisiting every raise site.
pub const fn sig_default_ignores(signum: u8) -> bool {
    matches!(sig_default_action(signum), SigDefault::Ignore)
}

// =============================================================================
// Signal frame — saved on user stack during signal delivery
// =============================================================================

/// Signal frame pushed onto the user stack when delivering a signal.
/// rt_sigreturn restores execution state from this frame.
///
/// The restorer address is NOT part of this frame — it is pushed as a
/// separate 8-byte word on the stack before the frame (Linux convention).
/// When the handler does `ret`, it pops the restorer into RIP, and RSP
/// then points to this SignalFrame, which rt_sigreturn reads directly.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct SignalFrame {
    /// Signal number being delivered.
    pub signum: u64,
    /// Saved general-purpose registers.
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    /// Saved instruction pointer (where to resume after sigreturn).
    pub rip: u64,
    /// Saved flags register.
    pub rflags: u64,
    /// Saved signal mask (restored by sigreturn).
    pub saved_mask: SigSet,
}
