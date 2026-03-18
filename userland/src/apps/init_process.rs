use slopos_abi::syscall::BOOT_FLAG_ROULETTE_SKIP;

use crate::program_registry;
use crate::syscall::{core as sys_core, process, UserSysInfo};

fn spawn_service(name: &str) -> i32 {
    let tid = match program_registry::resolve_program(name) {
        Some(spec) => {
            process::spawn_path_with_attrs(spec.path.as_bytes(), spec.priority, spec.flags)
        }
        None => -1,
    };
    if tid <= 0 {
        eprintln!("init: failed to spawn service");
    }
    tid
}

pub fn init_user_main() {
    let mut info = UserSysInfo::default();
    let skip_roulette =
        sys_core::sys_info(&mut info) == 0 && (info.boot_flags & BOOT_FLAG_ROULETTE_SKIP) != 0;

    if !skip_roulette {
        let roulette_tid = spawn_service("roulette");
        if roulette_tid > 0 {
            process::waitpid(roulette_tid as u32);
        }
    }

    let compositor_tid = spawn_service("compositor");
    spawn_service("shell");

    // Block on compositor — it runs forever so init stays dormant (zero CPU).
    // Like real PID 1: wait for children, don't busy-loop.
    if compositor_tid > 0 {
        process::waitpid(compositor_tid as u32);
    }

    // Compositor died — keep init alive as a fallback reaper.
    loop {
        sys_core::yield_now();
    }
}
