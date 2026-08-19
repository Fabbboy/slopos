//! `/bin/halt` — the one program the kernel confers `Power` on.
//!
//! Power is not a shell builtin here, for the same reason it is not one in
//! Linux, Redox or Asterinas. `reboot(2)` needs `CAP_SYS_BOOT`, which a shell
//! does not hold: `/sbin/halt` is a separate privileged binary, and
//! `systemctl poweroff` asks logind over D-Bus rather than doing it. Redox
//! makes every such resource a scheme backed by a daemon that holds the
//! authority. All three say the same thing — the shell asks something that
//! *has* the authority; it never holds it.
//!
//! SlopOS's mechanism for "something that has the authority" is program
//! identity, so this is that program. The shell spawns it and waits.
//!
//! One binary rather than two so the grant table names one path: `argv[0]`
//! selects the action, which is how `halt`/`reboot`/`poweroff` have always
//! been one executable with several names.

use crate::syscall::core as sys_core;
use crate::syscall::process;

fn basename(arg: &str) -> &str {
    arg.rsplit('/').next().unwrap_or(arg)
}

pub fn halt_user_main() {
    // `argv[0]` is the requested action. The shell passes the name the user
    // typed, so `reboot` and `halt` reach the same binary.
    let mut args = std::env::args();
    let action = args
        .next()
        .map(|a| basename(&a).to_string())
        .unwrap_or_else(|| "halt".to_string());

    // A second argument overrides, so `halt reboot` works from a script that
    // cannot control argv[0].
    let action = args.next().unwrap_or(action);

    match action.as_str() {
        "reboot" => {
            println!("Rebooting...");
            process::reboot();
        }
        "halt" | "poweroff" | "shutdown" => {
            println!("Halting...");
            process::halt();
        }
        other => {
            eprintln!("halt: unknown action '{other}' (want halt, poweroff or reboot)");
            sys_core::exit_with_code(1);
        }
    }
}
