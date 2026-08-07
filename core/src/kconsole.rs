//! The diagnostic console's own command.
//!
//! `h` describes the registry rather than any one subsystem, so it has no
//! natural owner among the crates that contribute commands. It lives here
//! because registration must happen in a crate only the kernel links: OSTD
//! defines the registry but is also linked into userland binaries, whose
//! linker script brackets no kernel section.

use slopos_ostd::kconsole::{KCMD_INFORMATIONAL, KConsole, help_body};
use slopos_ostd::kline;
use slopos_ostd::sync::lock_graph as lg;
use slopos_ostd::watchdog::{self, ChainEnd, MAX_WAIT_HOPS, WaitHop};

slopos_ostd::kcommand! {
    name = help,
    key = b'h',
    help = "list commands",
    flags = KCMD_INFORMATIONAL,
    run = run_help,
}

slopos_ostd::kcommand! {
    name = stalls,
    key = b'k',
    help = "worst observed stall per CPU and the live wait-for chains",
    flags = KCMD_INFORMATIONAL,
    run = run_stalls,
}

slopos_ostd::kcommand! {
    name = locks,
    key = b'd',
    help = "lock-order graph occupancy (needs lockdep=warn to be populated)",
    flags = KCMD_INFORMATIONAL,
    run = run_locks,
}

slopos_ostd::kcommand! {
    name = probe,
    key = b'p',
    help = "NMI every other CPU for its registers; unwind this one",
    flags = KCMD_INFORMATIONAL,
    run = run_probe,
}

fn run_help(kc: &mut KConsole<'_>) {
    help_body(kc);
}

fn run_probe(kc: &mut KConsole<'_>) {
    slopos_ostd::kconsole::probe::fan_out(kc);
}

/// The watchdog's view: who has been stalled, and who is waiting on whom now.
///
/// Both halves matter and answer different questions. `max_stall` is history —
/// the worst this machine has ever been — and the wait-for chain is the
/// present, which is the half that names a deadlock while it is happening.
fn run_stalls(kc: &mut KConsole<'_>) {
    kline!(
        kc,
        "watchdog: enabled={} miss_threshold={}",
        watchdog::is_enabled(),
        watchdog::miss_threshold()
    );

    let cpus = slopos_ostd::cpu::x86_64::pcr::get_cpu_count();
    let mut any_stall = false;
    for watcher in 0..cpus {
        if let Some((target, samples)) = watchdog::max_stall(watcher) {
            any_stall = true;
            kline!(
                kc,
                "  cpu {} worst observed stall {} samples (watcher cpu {})",
                target,
                samples,
                watcher
            );
        }
    }
    if !any_stall {
        // A command that prints nothing is indistinguishable from a broken
        // one, so "no stalls" is stated rather than implied.
        kline!(kc, "  no CPU has ever been observed stalled");
    }

    let mut hops = [WaitHop {
        cpu: 0,
        seq: 0,
        lock: 0,
    }; MAX_WAIT_HOPS];
    for cpu in 0..cpus {
        let (len, end) = watchdog::wait_chain_snapshot(cpu, &mut hops);
        if end == ChainEnd::NotWaiting {
            continue;
        }
        kline!(kc, "  cpu {} wait-for chain:", cpu);
        for hop in &hops[..len] {
            kline!(kc, "    cpu {} waits on lock {:#x}", hop.cpu, hop.lock);
        }
        kline!(
            kc,
            "    {}",
            match end {
                ChainEnd::Cycle => "chain closes on itself — deadlock cycle",
                ChainEnd::Truncated => "chain truncated, no cycle within the bound",
                ChainEnd::HolderUnknown => "chain ends: holder unknown",
                ChainEnd::NotWaiting => unreachable!(),
            }
        );
    }
}

/// Lock-order validator occupancy.
///
/// Emitted through the console rather than by calling `kdiag_dump_lock_graph`,
/// which writes at a level the boot may have filtered out and which a CI gate
/// parses — a diagnostic an operator asked for should not depend on the log
/// level, and should not put a second copy of a gated line on the wire.
fn run_locks(kc: &mut KConsole<'_>) {
    let classes = lg::class_count();
    let state = if !lg::tracking_enabled() {
        "OFF (tracking never enabled)"
    } else if lg::graph_overflowed() {
        "DISABLED (pool overflow)"
    } else if lg::fatal_bypassed() {
        "DISABLED (fatal bypass latched)"
    } else if lg::lockdep_mode() == lg::LockdepMode::Off {
        "OFF (lockdep=off)"
    } else {
        "ACTIVE"
    };

    kline!(kc, "lockdep: {} mode={:?}", state, lg::lockdep_mode());
    kline!(
        kc,
        "  classes={}/{} edges={}/{} chains={}/{} held_max={}/{}",
        classes,
        lg::REGISTRABLE_CLASSES,
        lg::edge_count(),
        lg::MAX_EDGES,
        lg::chain_count(),
        lg::MAX_CHAINS,
        lg::held_depth_max(),
        lg::MAX_HELD_LOCKS
    );
    kline!(
        kc,
        "  violations={} reports={} collisions={} chain_hit={} chain_miss={} held_drops={}",
        lg::violations_reported(),
        lg::violation_reports(),
        lg::class_collisions(),
        lg::chain_hits(),
        lg::chain_misses(),
        lg::held_depth_overflows()
    );

    if classes == 0 {
        kline!(
            kc,
            "  no classes registered — boot with lockdep=warn to populate the graph"
        );
        return;
    }

    // The class table is the resource that overflows first, and a *run of
    // contiguous addresses* is what identifies the array-of-locks static that
    // ate it — which a single name cannot show. Bounded by the line budget.
    for idx in 0..classes {
        if kc.budget_left() == 0 {
            return;
        }
        let Some(c) = lg::class_info(idx) else {
            continue;
        };
        kline!(
            kc,
            "  class {:>3}: {}{} ({}) level {}{} first-inst {:#x}",
            idx,
            c.name,
            if c.subclass != 0 { "/nested" } else { "" },
            c.site,
            c.level,
            if c.flags & lg::LO_DUPOK != 0 {
                " DUPOK"
            } else {
                ""
            },
            c.first_addr
        );
    }
}
