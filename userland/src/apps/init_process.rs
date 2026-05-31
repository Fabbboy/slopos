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
    upgrade_console_font();

    let mut info = UserSysInfo::default();
    let info_ok = sys_core::sys_info(&mut info) == 0;
    let skip_roulette = info_ok && (info.boot_flags & BOOT_FLAG_ROULETTE_SKIP) != 0;
    let tests_enabled = info_ok && (info.boot_flags & BOOT_FLAG_TESTS_ENABLED) != 0;

    if tests_enabled {
        // Drive the kernel-side userland-test phase from this task's
        // context. The syscall handler walks the `.test_registry`,
        // spawns each utest binary, blocks on its exit via
        // `task_wait_for`, drains its `SYSCALL_TEST_REPORT` ring, emits
        // KTAP, merges with the kernel-phase summary, and triggers the
        // QEMU shutdown. On success it returns; we then exit so the
        // boot pipeline doesn't keep the system running waiting for
        // input. On failure we fall through to the normal init flow so
        // the user can still reach a shell to investigate.
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
    let compositor_tid = spawn_service("compositor");

    // Root supervision loop on the slopfut runtime: await the compositor's
    // readiness byte (the gate read folded in per §1.1), spawn the shell,
    // then await the compositor's exit via a pidfd (`Child::wait`) instead
    // of a blocking `waitpid`. A small ring suffices — peak in-flight is one
    // gate read followed by one pidfd poll.
    match Ring::setup(8) {
        Ok(ring) => {
            slopfut::block_on(ring, async {
                if let Some(gate) = gate {
                    gate.wait_async().await;
                }

                spawn_service("shell");

                if compositor_tid > 0 {
                    let _ = slopfut::process::Child::from_pid(compositor_tid as u32)
                        .wait()
                        .await;
                }
            });
        }
        Err(_) => {
            // Ring unavailable: fall back to the synchronous boot path so
            // init never silently stalls.
            if let Some(gate) = gate {
                gate.wait();
            }
            spawn_service("shell");
            if compositor_tid > 0 {
                process::waitpid(compositor_tid as u32);
            }
        }
    }

    loop {
        sys_core::yield_now();
    }
}
