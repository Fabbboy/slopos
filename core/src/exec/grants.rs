//! Program-identity privilege grants.
//!
//! `task.flags` is the whole of SlopOS's privilege model, so the only question
//! that matters at a spawn is where the privileged bits come from. They come
//! from here: a fixed table keyed on the path of the program being loaded,
//! applied by [`spawn_program_with_attrs`](super::spawn_program_with_attrs)
//! *after* the syscall boundary has already stripped every privileged bit the
//! caller asked for. The syscall boundary is purely subtractive — it can reject
//! or strip, never confer.
//!
//! Keying on the program rather than on the caller is what keeps the three
//! legitimate launch paths working. `roulette` is started by init, by the shell
//! and by the compositor's shelf; under a "a caller may pass a bit it already
//! holds" rule the last two fail, because neither the shell nor the compositor
//! holds `DISPLAY_EXCLUSIVE` — and the first fails too, because init holds
//! `SYSTEM`, not `DISPLAY_EXCLUSIVE`. Keying on the program also closes a
//! channel that rule would open: a task holding `COMPOSITOR` could otherwise
//! stamp it onto any binary it launches.
//!
//! `SYSTEM` deliberately appears nowhere below. `/sbin/init` gets it from
//! `launch_init` and utest binaries from the kernel-side runner — both kernel
//! callers, neither reachable from a syscall. Naming `/sbin/init` here would
//! let any task re-spawn it and inherit console administration.
//!
//! This is containment, not a privilege model. It is only as strong as write
//! protection on `/bin`, and SlopOS has no file permissions — a task that can
//! overwrite `/bin/roulette` still obtains `DISPLAY_EXCLUSIVE`. That is
//! strictly narrower than accepting any bit from any caller for any binary.

use slopos_abi::task::{TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TaskPriority};

/// One program's kernel-conferred attributes.
struct ProgramGrant {
    /// The exact path the spawn must name, compared byte-for-byte against the
    /// NUL-trimmed request. `/bin/./roulette` and `//bin/roulette` do not
    /// match; failing closed on a non-canonical spelling costs a launcher
    /// nothing (every in-tree caller passes a static path literal from the
    /// program registry) and keeps this from becoming a parser.
    path: &'static [u8],
    /// Flags OR-ed into the child's flag word.
    flags: u16,
    /// A scheduling tier that replaces the caller's request, for a program that
    /// needs one user space may not ask for.
    priority: Option<TaskPriority>,
}

const PROGRAM_GRANTS: &[ProgramGrant] = &[
    // The compositor owns the screen and installs the global input sink, and it
    // is the one user task whose latency is a correctness property: every other
    // GUI client's frames land through it. `High` is a tier the syscall
    // boundary refuses from anybody.
    ProgramGrant {
        path: b"/bin/compositor",
        flags: TASK_FLAG_COMPOSITOR,
        priority: Some(TaskPriority::High),
    },
    // The Wheel of Fate draws straight to the framebuffer before a compositor
    // exists, which is exactly what `roulette_draw`'s `requires(display_exclusive)`
    // gates.
    ProgramGrant {
        path: b"/bin/roulette",
        flags: TASK_FLAG_DISPLAY_EXCLUSIVE,
        priority: None,
    },
];

/// The flags and tier the kernel adds for `path`: `(0, None)` for every program
/// not named above, which is all of them but two.
pub fn grant_for(path: &[u8]) -> (u16, Option<TaskPriority>) {
    match PROGRAM_GRANTS.iter().find(|grant| grant.path == path) {
        Some(grant) => (grant.flags, grant.priority),
        None => (0, None),
    }
}
