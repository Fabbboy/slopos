//! Program-identity privilege grants.
//!
//! `task.flags` is the whole of SlopOS's privilege model. The privileged bits
//! come from a fixed table keyed on the program's path, applied by
//! [`spawn_program_with_attrs`](super::spawn_program_with_attrs) *after* the
//! syscall boundary stripped every privileged bit the caller asked for: that
//! boundary is purely subtractive — it can reject or strip, never confer.
//!
//! `SYSTEM` deliberately appears nowhere below; naming `/sbin/init` here would
//! let any task re-spawn it and inherit console administration.
//!
//! This is containment, not a privilege model: it is only as strong as write
//! protection on `/bin`, which SlopOS does not have.

use slopos_abi::task::{
    TASK_FLAG_COMPOSITOR, TASK_FLAG_CONSOLE_ADMIN, TASK_FLAG_DISPLAY_EXCLUSIVE,
    TASK_FLAG_NET_ADMIN, TASK_FLAG_POWER, TASK_FLAG_PROC_ADMIN, TaskPriority,
};

struct ProgramGrant {
    /// Compared byte-for-byte against the NUL-trimmed request: a non-canonical
    /// spelling fails closed rather than making this a parser.
    path: &'static [u8],
    /// OR-ed into the child's flag word.
    flags: u16,
    /// Replaces the caller's requested tier, for a program needing one user
    /// space may not ask for.
    priority: Option<TaskPriority>,
}

const PROGRAM_GRANTS: &[ProgramGrant] = &[
    // Every other GUI client's frames land through the compositor, so its
    // latency is a correctness property. `High` is a tier the syscall boundary
    // refuses from anybody.
    ProgramGrant {
        path: b"/bin/compositor",
        flags: TASK_FLAG_COMPOSITOR,
        priority: Some(TaskPriority::High),
    },
    // Draws straight to the framebuffer before a compositor exists — what
    // `roulette_draw`'s `requires(display_exclusive)` gates.
    ProgramGrant {
        path: b"/bin/roulette",
        flags: TASK_FLAG_DISPLAY_EXCLUSIVE,
        priority: None,
    },
    // The one writer of the kernel keyboard layout, a single global table
    // feeding every TTY and the compositor; reading it needs nothing.
    ProgramGrant {
        path: b"/bin/keymap",
        flags: TASK_FLAG_CONSOLE_ADMIN,
        priority: None,
    },
    // Every mutating net syscall is gated on this bit, so the control plane
    // has one grammar.
    ProgramGrant {
        path: b"/bin/ip",
        flags: TASK_FLAG_NET_ADMIN,
        priority: None,
    },
    // May enumerate past the dominance relation `process_list` otherwise
    // applies, but gains no power over what it sees: `kill` re-checks dominance
    // against the caller's own flags.
    ProgramGrant {
        path: b"/bin/sysmon",
        flags: TASK_FLAG_PROC_ADMIN,
        priority: None,
    },
    // The only program that may halt or reboot. Power is deliberately not a
    // shell builtin — Linux gates `reboot(2)` on `CAP_SYS_BOOT` and ships
    // `/sbin/halt` separately, `systemctl poweroff` asks logind, and Redox puts
    // the resource behind a daemon. The shell spawns this and waits.
    ProgramGrant {
        path: b"/bin/halt",
        flags: TASK_FLAG_POWER,
        priority: None,
    },
];

/// The flags and tier the kernel adds for `path`; `(0, None)` for any program
/// not named above.
pub fn grant_for(path: &[u8]) -> (u16, Option<TaskPriority>) {
    match PROGRAM_GRANTS.iter().find(|grant| grant.path == path) {
        Some(grant) => (grant.flags, grant.priority),
        None => (0, None),
    }
}
