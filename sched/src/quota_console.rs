//! The `ledger` diagnostic command: what each account is holding, plus the
//! `QUOTA[<phase>]` report line the headroom gate parses.
//!
//! Registered from `sched` rather than from OSTD, which is also linked into
//! userland binaries whose linker script brackets no kernel section.

use slopos_abi::quota::{QuotaMode, ResourceKind};
use slopos_ostd::kconsole::{KCMD_INFORMATIONAL, KConsole};
use slopos_ostd::kline;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{
    LedgerFault, charge_audit_entries, for_each_account, ledger_audit, quota_mode, stats,
};

slopos_ostd::kcommand! {
    name = ledger,
    key = b'q',
    help = "resource accounts: used/peak/limit/denials per kind, and zombie rows",
    flags = KCMD_INFORMATIONAL,
    run = run_ledger,
}

fn describe(kc: &mut KConsole<'_>, fault: LedgerFault) {
    match fault {
        LedgerFault::AncestorUnderCount {
            ancestor,
            kind,
            ancestor_used,
            children_used,
        } => kline!(
            kc,
            "  FAULT slot={} {}: holds {} but its children hold {} — a refund \
             reached a descendant and not this level",
            ancestor.slot(),
            kind.name(),
            ancestor_used,
            children_used
        ),
        LedgerFault::UsedAbovePeak {
            account,
            kind,
            used,
            peak,
        } => kline!(
            kc,
            "  FAULT slot={} {}: used {} exceeds peak {} — a debit landed \
             without going through try_charge",
            account.slot(),
            kind.name(),
            used,
            peak
        ),
        LedgerFault::OverLimit {
            account,
            kind,
            used,
            limit,
        } => kline!(
            kc,
            "  FAULT slot={} {}: used {} is over its ceiling of {}",
            account.slot(),
            kind.name(),
            used,
            limit
        ),
        LedgerFault::PagesMismatch {
            account,
            mapped,
            charged,
            used,
        } => kline!(
            kc,
            "  FAULT slot={} pages: maps {}, tokens claim {}, row holds {} — a \
             region changed without its charge, or a debit landed without a token",
            account.slot(),
            mapped,
            charged,
            used
        ),
    }
}

/// The runtime form of the ledger's equality invariant; returns the number of
/// faults found.
///
/// The only mechanism that can see a forgotten or unwinder-skipped charge: the
/// type system guarantees the token is unique, never that the number matches
/// reality.
pub fn quotacheck(mut emit: impl FnMut(LedgerFault)) -> usize {
    ledger_audit(&mut emit)
}

fn mode_name(mode: QuotaMode) -> &'static str {
    match mode {
        QuotaMode::Off => "off",
        QuotaMode::Warn => "warn",
        QuotaMode::Enforce => "enforce",
    }
}

/// Whether a live process still stands behind `account`.
///
/// A row with non-zero numbers and no live process is a **zombie row**: a
/// charge outliving the process that took it. Self-healing, because the slot's
/// next occupant draws a fresh generation and the stale refund is a no-op;
/// listed anyway, since it is otherwise indistinguishable from a leak.
fn has_live_process(account: AccountId) -> bool {
    let mut found = false;
    crate::task::task_for_each_active(|task| {
        if found {
            return;
        }
        if let Some(process) = task.process()
            && process.account() == account
        {
            found = true;
        }
    });
    found
}

fn run_ledger(kc: &mut KConsole<'_>) {
    kline!(
        kc,
        "quota mode={} charge-bearing types={}",
        mode_name(quota_mode()),
        charge_audit_entries().len()
    );
    let faults = {
        let mut found = 0usize;
        ledger_audit(|fault| {
            found += 1;
            if found <= 8 {
                describe(kc, fault);
            }
        });
        found
    };
    kline!(
        kc,
        "quotacheck: {}",
        if faults == 0 {
            "consistent"
        } else {
            "INCONSISTENT (see FAULT lines above)"
        }
    );
    let mut zombies = 0u32;
    for_each_account(|account, parent| {
        if kc.budget_left() == 0 {
            return;
        }
        let live = has_live_process(account);
        let mut any = false;
        for kind in ResourceKind::ALL {
            let Some(s) = stats(account, kind) else {
                continue;
            };
            if s.used == 0 && s.peak == 0 && s.denials == 0 {
                continue;
            }
            if !any {
                kline!(
                    kc,
                    "account slot={} gen={} parent={} process={}",
                    account.slot(),
                    account.generation(),
                    if parent.is_none() {
                        -1i64
                    } else {
                        parent.slot() as i64
                    },
                    if live { "live" } else { "ZOMBIE" }
                );
                any = true;
            }
            kline!(
                kc,
                "  {:<12} used={:<6} peak={:<6} limit={:<10} denials={}",
                kind.name(),
                s.used,
                s.peak,
                if s.limit == slopos_ostd::process::quota::NO_LIMIT {
                    -1i64
                } else {
                    s.limit as i64
                },
                s.denials
            );
        }
        if any && !live {
            zombies += 1;
        }
    });
    kline!(
        kc,
        "zombie rows={} (charges outliving their process; self-healing on slot reuse)",
        zombies
    );
}

/// Emit one `QUOTA[<phase>]` line per account row carrying numbers — the wire
/// form `scripts/check_quota_headroom.sh` parses. Called at the same three
/// points the lockdep report is, so one boot answers both ratchets.
pub fn quota_report(phase: &str) {
    report_charge_cost();
    let mode = mode_name(quota_mode());
    for_each_account(|account, _| {
        for kind in ResourceKind::ALL {
            let Some(s) = stats(account, kind) else {
                continue;
            };
            if s.peak == 0 && s.denials == 0 {
                continue;
            }
            slopos_ostd::klog_info!(
                "QUOTA[{}]: mode={} slot={} kind={} used={} peak={} limit={} denials={}",
                phase,
                mode,
                account.slot(),
                kind.name(),
                s.used,
                s.peak,
                if s.limit == slopos_ostd::process::quota::NO_LIMIT {
                    -1i64
                } else {
                    s.limit as i64
                },
                s.denials
            );
        }
    });
}

/// Measured cost of one charge+refund round trip, in TSC cycles. Recorded by
/// the `test_quota_charge_cost` kernel test; zero means the test did not run.
static CHARGE_COST_NS: [core::sync::atomic::AtomicU64; 2] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 2];
static CHARGE_COST_DEPTH: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Record the measured per-charge cost at depth 1 and at `deep_depth`.
pub fn record_charge_cost(shallow_cycles: u64, deep_depth: u32, deep_cycles: u64) {
    use core::sync::atomic::Ordering;
    CHARGE_COST_NS[0].store(shallow_cycles, Ordering::Release);
    CHARGE_COST_NS[1].store(deep_cycles, Ordering::Release);
    CHARGE_COST_DEPTH.store(deep_depth, Ordering::Release);
}

fn report_charge_cost() {
    use core::sync::atomic::Ordering;
    let depth = CHARGE_COST_DEPTH.load(Ordering::Acquire);
    if depth == 0 {
        return;
    }
    slopos_ostd::klog_info!(
        "QUOTACOST: depth=1 cycles_per_charge={}",
        CHARGE_COST_NS[0].load(Ordering::Acquire)
    );
    slopos_ostd::klog_info!(
        "QUOTACOST: depth={} cycles_per_charge={}",
        depth,
        CHARGE_COST_NS[1].load(Ordering::Acquire)
    );
}
