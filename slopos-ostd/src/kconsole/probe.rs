//! The all-CPU register probe.
//!
//! Each CPU describes itself: a peer's registers are visible only to it, and
//! its frame chain is live. The probe arms the watchdog's per-CPU disposition
//! slot and sends an NMI, reusing the lockup detector's path rather than adding
//! code that runs inside an NMI handler — a *returning* NMI handler must take
//! no fault, and every stack walker in the tree is fault-recoverable rather
//! than fault-free.
//!
//! The calling CPU is skipped: a self-NMI is the one case guaranteed to land on
//! a CPU holding the klog ticket the emitters underneath this command use.

use crate::cpu::x86_64::pcr;
use crate::kconsole::KConsole;
use crate::kline;
use crate::watchdog::{self, NmiDisposition};

/// Spin rounds per millisecond of `probe_ms`.
///
/// The wait cannot read a clock: the console must work on a machine whose timer
/// is part of the problem. A spin count is a coarse budget, which is all this
/// needs — the answer arrives in microseconds or not at all.
const ROUNDS_PER_MS: u32 = 20_000;

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

    // The handler releases the slot as its last act, so the slot leaving Probe
    // is the answer arriving; its output reaches the console through the
    // NMI-safe emitters, not this handle.
    let budget = u32::from(probe_ms).saturating_mul(ROUNDS_PER_MS);
    for _ in 0..budget {
        if watchdog::probe_disposition(cpu) != NmiDisposition::Probe {
            return;
        }
        core::hint::spin_loop();
    }

    // Deliberately not released: the watchdog may have armed Fatal on this slot
    // since, and clearing that would let the stale NMI arrive at a slot re-armed
    // to kill the machine. The next probe reaps it.
    kline!(
        kc,
        "  cpu {}: no answer in {} ms — probe left armed",
        cpu,
        probe_ms
    );
}

/// Deliberately no backtrace: this CPU's stack is the console's own plumbing
/// every time, and printing it would cost a frame large enough to need a
/// stack-gate exemption.
fn describe_self(kc: &mut KConsole<'_>, me: usize) {
    kline!(kc, "  cpu {}: running the console, so not stalled", me);
}
