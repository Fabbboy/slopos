//! The `ledger` diagnostic command: what each account is holding.
//!
//! Registered from `sched` rather than from OSTD, which defines the arena but
//! is also linked into userland binaries whose linker script brackets no
//! kernel section. `sched` owns process identity, which is what an account
//! row is keyed on.
//!
//! Also the emitter of the `QUOTA[<phase>]` report line the headroom gate
//! parses, for the same reason the lockdep report lives beside its data.

use slopos_abi::quota::{QuotaMode, ResourceKind};
use slopos_ostd::kconsole::{KCMD_INFORMATIONAL, KConsole};
use slopos_ostd::kline;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{for_each_account, quota_mode, stats};

slopos_ostd::kcommand! {
    name = ledger,
    key = b'q',
    help = "resource accounts: used/peak/limit/denials per kind, and zombie rows",
    flags = KCMD_INFORMATIONAL,
    run = run_ledger,
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
/// A row whose numbers are non-zero with no live process is a **zombie row**:
/// a charge outliving the process that took it — an in-flight `SCM_RIGHTS`
/// reference, a keepalive pin the NIC has not reclaimed. Bounded and
/// self-healing, because the slot's next occupant draws a fresh generation and
/// the stale refund is a no-op. Listed anyway: a residual risk nobody can see
/// is indistinguishable from a leak.
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
    kline!(kc, "quota mode={}", mode_name(quota_mode()));
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

/// Emit one `QUOTA[<phase>]` line per account row carrying numbers.
///
/// The wire form `scripts/check_quota_headroom.sh` parses. Called at the same
/// three points the lockdep report is, so one boot answers both ratchets.
pub fn quota_report(phase: &str) {
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
