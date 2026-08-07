//! The all-CPU register probe.
//!
//! # Why each CPU describes itself
//!
//! Walking a peer CPU's stack from here would need its registers, which only
//! it can see, and would follow a frame chain it is actively changing. So the
//! probe asks: it arms the watchdog's per-CPU disposition slot and sends an
//! NMI, and the target's own handler emits its context.
//!
//! That path already exists — it is how the lockup detector gets a stalled
//! CPU's registers — so this adds no code that runs inside an NMI handler. It
//! could not: a *returning* NMI handler must take no fault, because the
//! `iretq` of that fault would unblock NMI while the handler is still running,
//! and every stack walker in the tree is fault-*recoverable* rather than
//! fault-free.
//!
//! # Why this CPU is skipped
//!
//! Sending ourselves an NMI is the one case guaranteed to land on a CPU
//! holding the klog ticket — the lock the emitters underneath this very
//! command are using. Nothing is lost by skipping it: a CPU running the
//! console is demonstrably making progress.

use crate::cpu::x86_64::pcr;
use crate::kconsole::KConsole;
use crate::kline;
use crate::watchdog::{self, NmiDisposition};

/// Spin rounds per millisecond of `probe_ms`.
///
/// The wait cannot read a clock: it runs with interrupts enabled but the
/// target may be anywhere, and the console must work on a machine whose timer
/// is part of the problem. A spin count is a coarse budget, which is all this
/// needs — the answer arrives in microseconds or not at all.
const ROUNDS_PER_MS: u32 = 20_000;

/// Probe every other CPU, then describe this one.
pub fn fan_out(kc: &mut KConsole<'_>) {
    let me = pcr::get_current_cpu();
    let count = pcr::get_cpu_count();
    let probe_ms = crate::kconsole::policy().probe_ms;

    kline!(kc, "probe: {} cpus, self is cpu {}", count, me);

    for cpu in 0..count {
        if cpu == me {
            continue;
        }
        probe_one(kc, cpu, probe_ms);
    }

    describe_self(kc, me);
}

/// Arm, NMI, and wait for one CPU to answer.
fn probe_one(kc: &mut KConsole<'_>, cpu: usize, probe_ms: u16) {
    // Reap a probe a previous round left armed. The exchange is conditional so
    // a slot the watchdog has since claimed for its own report stays its own.
    watchdog::release_probe_if(cpu, NmiDisposition::Probe);

    if !watchdog::arm_probe(cpu, NmiDisposition::Probe) {
        kline!(
            kc,
            "  cpu {}: busy — the watchdog or the TLB ladder is already probing it",
            cpu
        );
        return;
    }

    let Some(apic_id) = pcr::apic_id_from_cpu_index(cpu) else {
        watchdog::release_probe_if(cpu, NmiDisposition::Probe);
        kline!(kc, "  cpu {}: no APIC id", cpu);
        return;
    };
    pcr::send_nmi_to_cpu(apic_id);

    // The handler releases the slot as its last act, so the slot returning to
    // Unsolicited is the answer arriving. Its output goes straight to the
    // console through the NMI-safe emitters, not through this handle.
    let budget = u32::from(probe_ms).saturating_mul(ROUNDS_PER_MS);
    for _ in 0..budget {
        if watchdog::probe_disposition(cpu) != NmiDisposition::Probe {
            return;
        }
        core::hint::spin_loop();
    }

    // Deliberately not released. Between here and a release the watchdog may
    // have armed Fatal on this slot, and clearing that would let our stale NMI
    // arrive at a slot re-armed to kill the machine. The leak self-heals when
    // the target eventually takes the NMI, and the next probe reaps it.
    kline!(
        kc,
        "  cpu {}: no answer in {} ms — probe left armed",
        cpu,
        probe_ms
    );
}

/// Account for the CPU running the command.
///
/// Deliberately no backtrace. This CPU's stack is the console's own plumbing —
/// the drain, the dispatcher, this function — every time, because that is the
/// only way to reach here. Printing it would cost a frame large enough to need
/// a stack-gate exemption in exchange for four lines that say nothing about
/// the machine. That it is running at all is the finding.
fn describe_self(kc: &mut KConsole<'_>, me: usize) {
    kline!(kc, "  cpu {}: running the console, so not stalled", me);
}
