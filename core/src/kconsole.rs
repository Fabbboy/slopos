//! The diagnostic console's own command.
//!
//! `h` describes the registry rather than any one subsystem, so it has no
//! natural owner among the crates that contribute commands. It lives here
//! because registration must happen in a crate only the kernel links: OSTD
//! defines the registry but is also linked into userland binaries, whose
//! linker script brackets no kernel section.

use slopos_ostd::kconsole::{KCMD_INFORMATIONAL, KConsole, help_body};

slopos_ostd::kcommand! {
    name = help,
    key = b'h',
    help = "list commands",
    flags = KCMD_INFORMATIONAL,
    run = run_help,
}

fn run_help(kc: &mut KConsole<'_>) {
    help_body(kc);
}
