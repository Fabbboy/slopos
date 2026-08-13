//! The account arena: one `.bss` row per process plus the kernel's root.
//!
//! # No lock, and that is a property of the layout
//!
//! A lock on the charge path would take an inbound edge from every charge-site
//! class at once — the TCP shard, `UNIX_STATE`, `PROCESS_VMS`, a descriptor
//! table slot, the ring registry, memfd, signalfd, the vnode table — and any
//! path holding the account and then touching a subsystem lock would close a
//! cycle. Charging walks *up* a bounded chain of atomics, so it takes no locks
//! at all and cannot participate in one.
//!
//! # No release point
//!
//! A row is named by a generation-stamped [`AccountId`], never by a counted
//! reference. A counted reference inside a [`Charge`](super::Charge) would
//! make a refund a potential last release and therefore a heap free — and
//! refunds provably happen with interrupts off, under a cli-spinlock, and from
//! a dying task's own unwind. A `.bss` row has no release point, so the
//! question does not arise, and a refund against a released row is a defined
//! no-op rather than a write into a stranger's numbers.
//!
//! # No headroom predicate
//!
//! There is deliberately no `has_headroom(kind) -> bool`. A check-then-charge
//! split is a race by construction, and the reservation is the only
//! observation of headroom this module offers.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use slopos_abi::quota::{KIND_COUNT, QuotaMode, ResourceKind};

use super::axis::Refundable;
use super::token::Reservation;
use crate::process::AccountId;
use crate::process::account::{MAX_ACCOUNTS, ROOT_ACCOUNT_SLOT, root_account};

/// Rows on the longest permitted root-to-leaf chain.
///
/// Bounded so every hierarchical walk terminates and costs a fixed stack
/// frame, and creation at the bound is refused rather than silently re-homed.
/// The debit walk runs at most this many iterations, so the bound on chain
/// length and the bound on the walk are deliberately the same number: a chain
/// longer than the walk would have ancestors that never get debited, which is
/// a ceiling that silently does not apply.
pub const MAX_ACCOUNT_DEPTH: u8 = 8;

/// Levels a root may have beneath it: the chain length minus the root itself.
const ROOT_DEPTH_REMAINING: u8 = MAX_ACCOUNT_DEPTH - 1;

/// A limit that has not been configured. Distinct from a limit of zero, which
/// refuses everything.
pub const NO_LIMIT: u32 = slopos_abi::quota::NO_LIMIT_SENTINEL;

/// `parent` value meaning "no parent" — the root's own.
const NO_PARENT: u32 = u32::MAX;

/// One principal's numbers.
///
/// `peak` rather than a dump-time read of `used`: a dump samples whatever is
/// live at that instant, which is not the high-water mark the ceiling has to
/// be derived from. `denials` exists because a refusal nobody can see is a
/// silent denial, which is the failure mode this whole subsystem was written
/// to delete.
struct AccountRow {
    used: [AtomicU32; KIND_COUNT],
    limit: [AtomicU32; KIND_COUNT],
    peak: [AtomicU32; KIND_COUNT],
    denials: [AtomicU32; KIND_COUNT],
    /// Arena index of the account this one debits through. Written once at
    /// creation; there is no setter, which is what makes charge migration
    /// unrepresentable rather than merely discouraged.
    parent: AtomicU32,
    depth_remaining: AtomicU8,
    /// Matched against an [`AccountId`]'s generation before any row is
    /// touched. A mismatch is a stale designator and every operation on one is
    /// a no-op.
    generation: AtomicU64,
    live: AtomicBool,
}

impl AccountRow {
    const fn new() -> Self {
        Self {
            used: [const { AtomicU32::new(0) }; KIND_COUNT],
            limit: [const { AtomicU32::new(NO_LIMIT) }; KIND_COUNT],
            peak: [const { AtomicU32::new(0) }; KIND_COUNT],
            denials: [const { AtomicU32::new(0) }; KIND_COUNT],
            parent: AtomicU32::new(NO_PARENT),
            depth_remaining: AtomicU8::new(ROOT_DEPTH_REMAINING),
            generation: AtomicU64::new(0),
            live: AtomicBool::new(false),
        }
    }

    /// Zero every counter. Called on creation, so a recycled slot never shows
    /// its predecessor's numbers.
    fn reset_counters(&self) {
        for kind in 0..KIND_COUNT {
            self.used[kind].store(0, Ordering::Relaxed);
            self.limit[kind].store(NO_LIMIT, Ordering::Relaxed);
            self.peak[kind].store(0, Ordering::Relaxed);
            self.denials[kind].store(0, Ordering::Relaxed);
        }
    }
}

static ACCOUNTS: [AccountRow; MAX_ACCOUNTS] = [const { AccountRow::new() }; MAX_ACCOUNTS];

/// Refusal policy.
///
/// `Enforce` by default, now that the peaks have been measured and the
/// enforced ceilings derived from them. `quota=warn` remains the tier a *new*
/// kind's peaks are measured on — it grants an over-limit charge and counts
/// it, because a system that dies at its first over-limit cannot report what
/// its real high-water mark would have been.
static QUOTA_MODE: AtomicU8 = AtomicU8::new(mode_bits(QuotaMode::Enforce));

const fn mode_bits(mode: QuotaMode) -> u8 {
    match mode {
        QuotaMode::Off => 0,
        QuotaMode::Warn => 1,
        QuotaMode::Enforce => 2,
    }
}

/// Set the refusal policy. Boot cmdline only (`quota=off|warn|enforce`).
pub fn set_quota_mode(mode: QuotaMode) {
    QUOTA_MODE.store(mode_bits(mode), Ordering::Release);
}

/// The active refusal policy.
pub fn quota_mode() -> QuotaMode {
    match QUOTA_MODE.load(Ordering::Acquire) {
        0 => QuotaMode::Off,
        2 => QuotaMode::Enforce,
        _ => QuotaMode::Warn,
    }
}

/// Why a charge was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TryChargeError {
    /// The ancestor whose ceiling was reached — not necessarily the leaf, and
    /// naming it is what makes an over-limit diagnosable.
    pub refused_by: AccountId,
    pub kind: ResourceKind,
    pub errno: slopos_abi::Errno,
}

/// Why an account could not be created.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccountCreateError {
    /// The slot index is outside the arena.
    OutOfBounds,
    /// The parent is at [`MAX_ACCOUNT_DEPTH`], so a child of it would make a
    /// walk unbounded.
    TooDeep,
    /// The parent designator names no live row.
    NoParent,
}

// ---------------------------------------------------------------------------
// Row resolution
// ---------------------------------------------------------------------------

/// The live row `id` names, or `None` for a stale, absent or out-of-range one.
///
/// The generation compare is the whole mechanism: a refund arriving after its
/// account's slot was reused finds a generation that does not match and does
/// nothing, rather than debiting whichever principal holds that slot now.
fn row_for(id: AccountId) -> Option<&'static AccountRow> {
    if id.is_none() {
        return None;
    }
    let row = ACCOUNTS.get(id.slot() as usize)?;
    if id.slot() == ROOT_ACCOUNT_SLOT && !row.live.load(Ordering::Acquire) {
        ensure_root();
    }
    if !row.live.load(Ordering::Acquire) {
        return None;
    }
    if row.generation.load(Ordering::Acquire) != id.generation() {
        return None;
    }
    Some(row)
}

/// Materialise the root row, idempotently.
///
/// The root is the kernel's own payer and the ancestor every process account
/// debits through, so it has to exist before the first charge — which happens
/// during boot, before any explicit initialisation step could have run. The
/// limits start unset and are written later by [`set_limit`] once the frame
/// count has actually been measured.
fn ensure_root() {
    let id = root_account();
    let row = &ACCOUNTS[ROOT_ACCOUNT_SLOT as usize];
    if row.live.load(Ordering::Acquire) {
        return;
    }
    row.reset_counters();
    row.parent.store(NO_PARENT, Ordering::Relaxed);
    row.depth_remaining
        .store(ROOT_DEPTH_REMAINING, Ordering::Relaxed);
    row.generation.store(id.generation(), Ordering::Release);
    row.live.store(true, Ordering::Release);
}

/// The kernel's root account, with its row guaranteed to exist.
pub fn root() -> AccountId {
    ensure_root();
    root_account()
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Bind a row for `id`, debiting through `parent`.
///
/// The parent edge is written here and never again — the accounting tree *is*
/// the spawn tree, and no syscall mints an account.
pub fn account_create(id: AccountId, parent: AccountId) -> Result<(), AccountCreateError> {
    let slot = id.slot() as usize;
    if id.is_none() || slot >= MAX_ACCOUNTS {
        return Err(AccountCreateError::OutOfBounds);
    }

    let (parent_slot, depth) = if parent.is_none() {
        (NO_PARENT, ROOT_DEPTH_REMAINING)
    } else {
        let parent_row = row_for(parent).ok_or(AccountCreateError::NoParent)?;
        let remaining = parent_row.depth_remaining.load(Ordering::Acquire);
        if remaining == 0 {
            return Err(AccountCreateError::TooDeep);
        }
        (parent.slot(), remaining - 1)
    };

    let row = &ACCOUNTS[slot];
    row.reset_counters();
    // Every process account starts at the enforced per-kind defaults. The root
    // deliberately does not: it is the sum of every principal, so a
    // per-principal ceiling applied to it would refuse the machine's own
    // aggregate. Its limits are set from measured RAM at boot instead.
    if slot != ROOT_ACCOUNT_SLOT as usize {
        for kind in ResourceKind::ALL {
            row.limit[kind.index()].store(
                slopos_abi::quota::default_process_limit(kind),
                Ordering::Relaxed,
            );
        }
    }
    row.parent.store(parent_slot, Ordering::Relaxed);
    row.depth_remaining.store(depth, Ordering::Relaxed);
    // Generation before `live`: a reader checks liveness first, so this order
    // never exposes a live row carrying its predecessor's generation.
    row.generation.store(id.generation(), Ordering::Release);
    row.live.store(true, Ordering::Release);
    Ok(())
}

/// Release the row `id` names.
///
/// Outstanding amounts move one hop up the immutable parent chain — which is a
/// no-op on every ancestor, because the hierarchical debit already charged
/// them. The row itself goes dark, so a refund arriving later fails the
/// generation compare and does nothing. That is what makes a leaked charge
/// self-healing rather than a permanent lie.
pub fn account_release(id: AccountId) {
    let Some(row) = row_for(id) else {
        return;
    };
    row.live.store(false, Ordering::Release);
    row.generation.store(0, Ordering::Release);
    row.reset_counters();
}

/// Set the ceiling for one kind. Boot and test-fixture use only.
pub fn set_limit(id: AccountId, kind: ResourceKind, limit: u32) {
    if let Some(row) = row_for(id) {
        row.limit[kind.index()].store(limit, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Charge and refund
// ---------------------------------------------------------------------------

/// Debit `n` units of `A` from `account` and every ancestor.
///
/// The debit is applied leaf-upward. On refusal at level *k* every level
/// already debited is given back before returning, so a denied call is the
/// identity on every row — including the batch that succeeded for *k* and
/// failed at *k+1*, which is the case a hand-written cancel loop gets wrong.
///
/// A refusal against a stale or absent account succeeds vacuously: there is no
/// row to debit, so there is nothing to refuse, and returning an error would
/// make a process whose account had already been released unable to close its
/// own descriptors.
/// # Context
///
/// A charge belongs at a syscall entry point, where a principal is known and a
/// refusal has an errno to travel back on. A queue filled by a device IRQ or
/// by a remote peer must be **pre-charged at the syscall that created it**,
/// with an amount equal to its fixed capacity, so a full queue is a bound its
/// owner already paid for and dropping an event stops being an accounting
/// event.
///
/// That rule is deliberately not a runtime assertion. This kernel enters
/// syscalls through an interrupt gate, so `in_interrupt_context` is true on
/// every sanctioned charge site and false on none — an assertion over it fires
/// on exactly the callers it is meant to bless. Nothing else in the PCR
/// separates "a syscall that arrived via a trap gate" from "a device IRQ", so
/// the property is kept by where charges are written rather than by a check
/// that would have to be disabled to boot.
pub fn try_charge<A: Refundable>(
    account: AccountId,
    n: u32,
) -> Result<Reservation<A>, TryChargeError> {
    let kind = A::KIND;
    let mode = quota_mode();
    let mut charged: [u32; MAX_ACCOUNT_DEPTH as usize] = [NO_PARENT; MAX_ACCOUNT_DEPTH as usize];
    let mut depth = 0usize;

    let mut current = account;
    while depth < MAX_ACCOUNT_DEPTH as usize {
        let Some(row) = row_for(current) else {
            break;
        };
        if let Err(()) = charge_row(row, kind, n, mode) {
            unwind(&charged[..depth], kind, n);
            return Err(TryChargeError {
                refused_by: current,
                kind,
                errno: kind.errno(),
            });
        }
        charged[depth] = current.slot();
        depth += 1;

        let parent_slot = row.parent.load(Ordering::Acquire);
        if parent_slot == NO_PARENT {
            break;
        }
        current = account_id_at(parent_slot);
        if current.is_none() {
            break;
        }
    }

    Ok(Reservation::new(account, n))
}

/// Give `n` units of `kind` back to `account` and every ancestor.
///
/// Bounds-checked, generation-compared, and a defined no-op on mismatch. No
/// lock, no allocation, no wait — which is what makes it legal from every
/// context a destructor can run in.
pub(super) fn refund_raw(account: AccountId, kind: ResourceKind, n: u32) {
    if n == 0 {
        return;
    }
    let mut current = account;
    let mut depth = 0usize;
    while depth < MAX_ACCOUNT_DEPTH as usize {
        let Some(row) = row_for(current) else {
            return;
        };
        release_row(row, kind, n);
        let parent_slot = row.parent.load(Ordering::Acquire);
        if parent_slot == NO_PARENT {
            return;
        }
        current = account_id_at(parent_slot);
        if current.is_none() {
            return;
        }
        depth += 1;
    }
}

/// The live id occupying arena slot `slot`, or [`AccountId::NONE`].
///
/// Reading the generation back out of the row is what keeps the parent edge a
/// plain index: storing a full id would double the row's parent field for no
/// gain, and a parent whose row went dark answers `NONE` here, which stops the
/// walk exactly as it should.
fn account_id_at(slot: u32) -> AccountId {
    let Some(row) = ACCOUNTS.get(slot as usize) else {
        return AccountId::NONE;
    };
    if !row.live.load(Ordering::Acquire) {
        return AccountId::NONE;
    }
    AccountId::from_parts(slot, row.generation.load(Ordering::Acquire))
}

/// Debit one row, keeping `used <= limit` on every success.
///
/// A compare-exchange loop rather than `fetch_add`-then-check: an add that
/// overshoots and is corrected afterwards is observable, and "no successful
/// charge leaves `used` above `limit`" has to hold at every instant or it is
/// not the property the ceiling claims.
fn charge_row(row: &AccountRow, kind: ResourceKind, n: u32, mode: QuotaMode) -> Result<(), ()> {
    let idx = kind.index();
    // `Off` consults no ceiling at all, so it records no denial either: a
    // denial count that moved under `quota=off` would report refusals on a
    // tier that refuses nothing.
    let limit = match mode {
        QuotaMode::Off => NO_LIMIT,
        _ => row.limit[idx].load(Ordering::Acquire),
    };
    let mut used = row.used[idx].load(Ordering::Relaxed);
    let mut over_limit;
    loop {
        let Some(next) = used.checked_add(n) else {
            row.denials[idx].fetch_add(1, Ordering::Relaxed);
            return Err(());
        };
        over_limit = limit != NO_LIMIT && next > limit;
        if over_limit && matches!(mode, QuotaMode::Enforce) {
            row.denials[idx].fetch_add(1, Ordering::Relaxed);
            return Err(());
        }
        match row.used[idx].compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => {
                row.peak[idx].fetch_max(next, Ordering::Relaxed);
                // Counted even though the charge was granted: `quota=warn`
                // exists to measure what enforcement *would* have refused, and
                // a tier that reports nothing measures nothing.
                if over_limit {
                    row.denials[idx].fetch_add(1, Ordering::Relaxed);
                }
                return Ok(());
            }
            Err(observed) => used = observed,
        }
    }
}

/// Credit one row. Saturating: an under-run is a bookkeeping bug, and the
/// failure it must not produce is a count that wraps to `u32::MAX` and refuses
/// every subsequent charge forever.
fn release_row(row: &AccountRow, kind: ResourceKind, n: u32) {
    let idx = kind.index();
    let mut used = row.used[idx].load(Ordering::Relaxed);
    loop {
        let next = used.saturating_sub(n);
        debug_assert!(used >= n, "account row underflow on {}", kind.name());
        match row.used[idx].compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => used = observed,
        }
    }
}

/// Give back the levels a refused walk had already debited.
fn unwind(slots: &[u32], kind: ResourceKind, n: u32) {
    for &slot in slots {
        if let Some(row) = ACCOUNTS.get(slot as usize)
            && row.live.load(Ordering::Acquire)
        {
            release_row(row, kind, n);
        }
    }
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

/// One kind's numbers on one row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KindStats {
    pub used: u32,
    pub limit: u32,
    pub peak: u32,
    pub denials: u32,
}

/// Read one kind's numbers. `None` for a stale or absent account.
pub fn stats(id: AccountId, kind: ResourceKind) -> Option<KindStats> {
    let row = row_for(id)?;
    let idx = kind.index();
    Some(KindStats {
        used: row.used[idx].load(Ordering::Acquire),
        limit: row.limit[idx].load(Ordering::Acquire),
        peak: row.peak[idx].load(Ordering::Acquire),
        denials: row.denials[idx].load(Ordering::Acquire),
    })
}

/// Visit every live row, lowest slot first, with its id and parent.
///
/// The root sorts first because its slot is fixed at zero, so a dump reads
/// top-down without the walker sorting anything.
pub fn for_each_account(mut f: impl FnMut(AccountId, AccountId)) {
    ensure_root();
    for (slot, row) in ACCOUNTS.iter().enumerate() {
        if !row.live.load(Ordering::Acquire) {
            continue;
        }
        let id = AccountId::from_parts(slot as u32, row.generation.load(Ordering::Acquire));
        let parent = match row.parent.load(Ordering::Acquire) {
            NO_PARENT => AccountId::NONE,
            parent_slot => account_id_at(parent_slot),
        };
        f(id, parent);
    }
}

/// A violation of the ledger's own consistency, found by the runtime audit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerFault {
    /// An ancestor holds less than the descendants debiting through it. The
    /// hierarchical debit makes this impossible while every charge and refund
    /// walks the same chain, so it means a refund reached a level a charge did
    /// not — the phantom-refund shape the token exists to eliminate.
    AncestorUnderCount {
        ancestor: AccountId,
        kind: ResourceKind,
        ancestor_used: u32,
        children_used: u32,
    },
    /// `used` exceeds the high-water mark, which is impossible if every charge
    /// updates the peak: it means a debit landed without going through
    /// `charge_row`.
    UsedAbovePeak {
        account: AccountId,
        kind: ResourceKind,
        used: u32,
        peak: u32,
    },
    /// A row is above its own ceiling while enforcement is on — L2's step
    /// property failing.
    OverLimit {
        account: AccountId,
        kind: ResourceKind,
        used: u32,
        limit: u32,
    },
}

/// Check the ledger against itself, reporting every inconsistency to `report`.
///
/// The runtime form of the equality invariant, and the only mechanism that can
/// see a forgotten or unwinder-skipped charge. Three checks, each naming a
/// distinct way the numbers could be lying — see [`LedgerFault`].
///
/// Returns the number of faults found. Allocation-free: the walk is bounded by
/// the arena and by [`MAX_ACCOUNT_DEPTH`], so it is legal anywhere a read is.
pub fn ledger_audit(mut report: impl FnMut(LedgerFault)) -> usize {
    let enforcing = matches!(quota_mode(), QuotaMode::Enforce);
    let mut faults = 0usize;

    for (slot, row) in ACCOUNTS.iter().enumerate() {
        if !row.live.load(Ordering::Acquire) {
            continue;
        }
        let account = AccountId::from_parts(slot as u32, row.generation.load(Ordering::Acquire));

        for kind in ResourceKind::ALL {
            let idx = kind.index();
            let used = row.used[idx].load(Ordering::Acquire);
            let peak = row.peak[idx].load(Ordering::Acquire);
            let limit = row.limit[idx].load(Ordering::Acquire);

            if used > peak {
                faults += 1;
                report(LedgerFault::UsedAbovePeak {
                    account,
                    kind,
                    used,
                    peak,
                });
            }
            if enforcing && limit != NO_LIMIT && used > limit {
                faults += 1;
                report(LedgerFault::OverLimit {
                    account,
                    kind,
                    used,
                    limit,
                });
            }

            // Every direct child debits through this row, so this row's `used`
            // is at least their sum. Saturating, because a child's own charge
            // is included in its total and several children can each exceed
            // what a `u32` sum would hold.
            let mut children = 0u32;
            for (child_slot, child) in ACCOUNTS.iter().enumerate() {
                if child_slot == slot || !child.live.load(Ordering::Acquire) {
                    continue;
                }
                if child.parent.load(Ordering::Acquire) == slot as u32 {
                    children = children.saturating_add(child.used[idx].load(Ordering::Acquire));
                }
            }
            if used < children {
                faults += 1;
                report(LedgerFault::AncestorUnderCount {
                    ancestor: account,
                    kind,
                    ancestor_used: used,
                    children_used: children,
                });
            }
        }
    }
    faults
}

/// Live rows.
pub fn account_count() -> usize {
    ACCOUNTS
        .iter()
        .filter(|row| row.live.load(Ordering::Acquire))
        .count()
}

/// Credit exactly one row, skipping its ancestors. Test-fixture only.
///
/// Fabricates the inconsistency a hand-written cancel loop produces when it
/// misses a level, so the audit's own test can prove the audit rejects rather
/// than merely accepts.
#[cfg(test)]
pub(super) fn refund_raw_one_level_for_test(account: AccountId, kind: ResourceKind, n: u32) {
    // Writes the row directly rather than through `release_row`, whose
    // underflow `debug_assert` would fire on the very corruption being
    // planted.
    if let Some(row) = row_for(account) {
        let idx = kind.index();
        let used = row.used[idx].load(Ordering::Acquire);
        row.used[idx].store(used.saturating_sub(n), Ordering::Release);
    }
}

/// Inverse of [`refund_raw_one_level_for_test`], for restoring a row a test
/// deliberately corrupted so the real token's refund still balances.
#[cfg(test)]
pub(super) fn charge_raw_one_level_for_test(account: AccountId, kind: ResourceKind, n: u32) {
    if let Some(row) = row_for(account) {
        let idx = kind.index();
        let used = row.used[idx].load(Ordering::Acquire);
        row.used[idx].store(used.saturating_add(n), Ordering::Release);
    }
}

/// Release every row and restore the default policy. Test-fixture only.
///
/// The generation counter in `account.rs` deliberately survives, so an id
/// minted before a reset can never match the slot's next occupant.
pub fn reset_for_test() {
    for row in ACCOUNTS.iter() {
        row.live.store(false, Ordering::Release);
        row.generation.store(0, Ordering::Release);
        row.reset_counters();
    }
    set_quota_mode(QuotaMode::Enforce);
}
