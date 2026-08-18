use slopos_abi::syscall::{BOOT_FLAG_ROULETTE_SKIP, BOOT_FLAG_TESTS_ENABLED};
use slopos_font::atlas::GlyphAtlas;

use crate::program_registry;
use crate::readiness::ReadinessGate;
use crate::ring::{Ring, slopfut};
use crate::syscall::{UserSysInfo, core as sys_core, process, tty};

fn upgrade_console_font() {
    let font_data = match crate::gfx::font_loader::load_font("mono") {
        Some(data) => data,
        None => return,
    };
    let atlas = match GlyphAtlas::new(font_data, 16) {
        Some(a) => a,
        None => return,
    };
    let (coverage, replacement) = atlas.coverage_and_replacement();
    let mut payload = Vec::with_capacity(coverage.len() + replacement.len());
    payload.extend_from_slice(coverage);
    payload.extend_from_slice(replacement);
    tty::font_set_coverage(
        &payload,
        atlas.cell_width() as u16,
        atlas.cell_height() as u16,
    );
}

fn spawn_service(name: &str) -> i32 {
    spawn_service_inheriting(name, &[])
}

/// The child inherits the caller's stdio plus each fd in `extra_fds` via the
/// fd-action allow-list. At most two extra fds are honored.
fn spawn_service_inheriting(name: &str, extra_fds: &[i32]) -> i32 {
    let Some(spec) = program_registry::resolve_program(name) else {
        eprintln!("init: failed to spawn service");
        return -1;
    };
    let mut actions = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
        process::clone_fd(-1, -1),
        process::clone_fd(-1, -1),
    ];
    let base = 3;
    let n = extra_fds.len().min(actions.len() - base);
    for (slot, &fd) in actions[base..base + n].iter_mut().zip(extra_fds) {
        *slot = process::clone_fd(fd, fd);
    }
    let tid = process::spawn_path_with_actions(
        spec.path.as_bytes(),
        &[],
        spec.priority,
        spec.flags,
        &actions[..base + n],
        0,
    );
    if tid <= 0 {
        eprintln!("init: failed to spawn service");
    }
    tid
}

pub fn init_user_main() {
    upgrade_console_font();

    // Must precede anything interactive; a missing or invalid /etc/keymap
    // leaves the built-in US default active.
    crate::keymap::apply_persisted_layout();

    let mut info = UserSysInfo::default();
    let info_ok = sys_core::sys_info(&mut info) == 0;
    let skip_roulette = info_ok && (info.boot_flags & BOOT_FLAG_ROULETTE_SKIP) != 0;
    let tests_enabled = info_ok && (info.boot_flags & BOOT_FLAG_TESTS_ENABLED) != 0;

    if tests_enabled {
        // Exiting on success stops the boot pipeline waiting for input;
        // failure falls through to the normal flow so a shell is reachable.
        let rc = sys_core::run_userland_tests();
        if rc == 0 {
            sys_core::exit_with_code(0);
        }
    }

    if !skip_roulette {
        let roulette_tid = spawn_service("roulette");
        if roulette_tid > 0 {
            process::waitpid(roulette_tid as u32);
        }
    }

    let gate = ReadinessGate::create();
    // Without the inherited notifier (fd 3), init's gate read EOFs at once and
    // races the terminal ahead of a ready compositor. Clone it only when the
    // gate was actually set up.
    let compositor_tid = if gate.is_some() {
        spawn_service_inheriting("compositor", &[crate::readiness::NOTIFIER_FD])
    } else {
        spawn_service("compositor")
    };

    // Peak in-flight is one gate read followed by one pidfd poll, so a small
    // ring suffices.
    match Ring::setup(8) {
        Ok(ring) => {
            slopfut::block_on(ring, async {
                if let Some(gate) = gate {
                    gate.wait_async().await;
                }

                spawn_service("terminal");

                if compositor_tid > 0 {
                    let _ = slopfut::process::Child::from_pid(compositor_tid as u32)
                        .wait()
                        .await;
                }
            });
        }
        Err(_) => {
            if let Some(gate) = gate {
                gate.wait();
            }
            spawn_service("terminal");
            if compositor_tid > 0 {
                process::waitpid(compositor_tid as u32);
            }
        }
    }

    // Init outlives every service it starts, so nothing else ever reaps them:
    // yielding alone accumulates one zombie per terminal the user closes.
    loop {
        process::reap_exited_children();
        sys_core::yield_now();
    }
}
