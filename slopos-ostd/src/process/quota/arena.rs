//! The account arena: one `.bss` row per process plus the kernel's root.
//!
//! Charging walks *up* a bounded chain of atomics and takes no lock at all, so
//! it cannot close a cycle with the subsystem lock a charge site holds.
//!
//! A row is named by a generation-stamped [`AccountId`], never by a counted
//! reference: a counted reference inside a [`Charge`](super::Charge) would make
//! a refund a potential heap free, and refunds happen with interrupts off,
//! under a cli-spinlock, and from a dying task's own unwind. A `.bss` row has
//! no release point, and a refund against a released row is a defined no-op.
//!
//! There is deliberately no `has_headroom(kind) -> bool`: a check-then-charge
//! split is a race by construction, and the reservation is the only observation
//! of headroom this module offers.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use slopos_abi::quota::{KIND_COUNT, QuotaMode, ResourceKind};

use super::axis::Refundable;
use super::token::Reservation;
use crate::process::AccountId;
use crate::process::account::{MAX_ACCOUNTS, ROOT_ACCOUNT_SLOT, root_account};

/// Rows on the longest permitted root-to-leaf chain.
///
/// Bounded so every hierarchical walk terminates in a fixed stack frame, and
/// creation at the bound is refused rather than silently re-homed. The debit
/// walk runs at most this many iterations, so a longer chain would have
/// ancestors that never get debited — a ceiling that silently does not apply.
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
/// live at that instant, not the high-water mark a ceiling has to be derived
/// from. `denials` exists so that a refusal nobody can see is not silent.
struct AccountRow {
    /// `used` in the low half, `peak` in the high half: one compare-exchange
    /// installs both, so no reader can observe `used > peak`.
    usage: [AtomicU64; KIND_COUNT],
    limit: [AtomicU32; KIND_COUNT],
    denials: [AtomicU32; KIND_COUNT],
    /// Arena index of the account this one debits through. Written once at
    /// creation and never given a setter, which makes charge migration
    /// unrepresentable rather than merely discouraged.
    parent: AtomicU32,
    depth_remaining: AtomicU8,
    /// Matched against an [`AccountId`]'s generation before any row is touched.
    /// A mismatch is a stale designator and every operation on one is a no-op.
    generation: AtomicU64,
    live: AtomicBool,
}

impl AccountRow {
    const fn new() -> Self {
        Self {
            usage: [const { AtomicU64::new(0) }; KIND_COUNT],
            limit: [const { AtomicU32::new(NO_LIMIT) }; KIND_COUNT],
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
            self.usage[kind].store(0, Ordering::Relaxed);
            self.limit[kind].store(NO_LIMIT, Ordering::Relaxed);
            self.denials[kind].store(0, Ordering::Relaxed);
        }
    }
}

#[inline]
const fn pack_usage(used: u32, peak: u32) -> u64 {
    ((peak as u64) << 32) | used as u64
}

#[inline]
const fn usage_used(packed: u64) -> u32 {
    packed as u32
}

#[inline]
const fn usage_peak(packed: u64) -> u32 {
    (packed >> 32) as u32
}

static ACCOUNTS: [AccountRow; MAX_ACCOUNTS] = [const { AccountRow::new() }; MAX_ACCOUNTS];

/// Refusal policy.
///
/// `quota=warn` is the tier a *new* kind's peaks are measured on: it grants an
/// over-limit charge and counts it, because a system that dies at its first
/// over-limit cannot report what its real high-water mark would have been.
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
    /// The ancestor whose ceiling was reached — not necessarily the leaf.
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

/// The live row `id` names, or `None` for a stale, absent or out-of-range one.
///
/// The generation compare is the whole mechanism: a refund arriving after its
/// account's slot was reused finds a mismatch and does nothing, rather than
/// debiting whichever principal holds that slot now.
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
/// The root is the ancestor every process account debits through, so it has to
/// exist before the first charge — which happens during boot, before any
/// explicit initialisation step could have run. Limits start unset and are
/// written by [`set_limit`] once the frame count has been measured.
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
    // The root is deliberately exempt from the per-kind process defaults: it is
    // the sum of every principal, so a per-principal ceiling applied to it would
    // refuse the machine's own aggregate. Its limits come from measured RAM.
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
/// Outstanding amounts move one hop up the immutable parent chain. The row
/// itself goes dark, so a refund arriving later fails the generation compare
/// and does nothing — which is what makes a leaked charge self-healing.
pub fn account_release(id: AccountId) {
    let Some(row) = row_for(id) else {
        return;
    };

    // Dark first: the audit counts a live row among its parent's children, so
    // crediting the parent ahead of it dips the parent below their sum.
    row.live.store(false, Ordering::Release);

    // Before the outstanding amounts move up: inheriting the disk row would
    // bill the parent for blocks nothing can attribute to anyone.
    super::disk::release(id);

    let parent_slot = row.parent.load(Ordering::Acquire);
    if parent_slot != NO_PARENT {
        let parent = account_id_at(parent_slot);
        for kind in ResourceKind::ALL {
            let outstanding = usage_used(row.usage[kind.index()].load(Ordering::Acquire));
            if outstanding != 0 {
                refund_raw(parent, kind, outstanding);
            }
        }
    }

    row.generation.store(0, Ordering::Release);
    row.reset_counters();
}

/// Release whichever account currently occupies `slot`, whatever its
/// generation.
///
/// For the fixture reset, which clears the id space wholesale and has no live
/// [`AccountId`] to name each row with; ordinary teardown goes through the
/// generation-checked [`account_release`].
pub fn account_release_by_slot(slot: u32) {
    let Some(row) = ACCOUNTS.get(slot as usize) else {
        return;
    };
    if !row.live.load(Ordering::Acquire) {
        return;
    }
    account_release(AccountId::from_parts(
        slot,
        row.generation.load(Ordering::Acquire),
    ));
}

/// Set the ceiling for one kind. Boot and test-fixture use only.
pub fn set_limit(id: AccountId, kind: ResourceKind, limit: u32) {
    if let Some(row) = row_for(id) {
        row.limit[kind.index()].store(limit, Ordering::Release);
    }
}

/// Debit `n` units of `A` from `account` and every ancestor.
///
/// The debit is applied leaf-upward. On refusal at level *k* every level
/// already debited is given back before returning, so a denied call is the
/// identity on every row.
///
/// A refusal against a stale or absent account succeeds vacuously: there is no
/// row to debit, and returning an error would make a process whose account had
/// already been released unable to close its own descriptors.
///
/// # Context
///
/// A charge belongs at a syscall entry point, where a principal is known and a
/// refusal has an errno to travel back on. A queue filled by a device IRQ or
/// by a remote peer must be **pre-charged at the syscall that created it**,
/// with an amount equal to its fixed capacity, so a full queue is a bound its
/// owner already paid for.
///
/// That rule is deliberately not a runtime assertion: this kernel enters
/// syscalls through an interrupt gate, so `in_interrupt_context` is true on
/// every sanctioned charge site and false on none, and nothing else in the PCR
/// separates a syscall that arrived via a trap gate from a device IRQ.
pub fn try_charge<A: Refundable>(
    account: AccountId,
    n: u32,
) -> Result<Reservation<A>, TryChargeError> {
    let kind = A::KIND;
    let mode = quota_mode();
    let mut charged: [AccountId; MAX_ACCOUNT_DEPTH as usize] =
        [AccountId::NONE; MAX_ACCOUNT_DEPTH as usize];
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
        charged[depth] = current;
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
/// plain index; a parent whose row went dark answers `NONE` here, which stops
/// the walk exactly as it should.
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
/// overshoots and is corrected afterwards is observable, and the ceiling has to
/// hold at every instant or it is not the property it claims.
fn charge_row(row: &AccountRow, kind: ResourceKind, n: u32, mode: QuotaMode) -> Result<(), ()> {
    let idx = kind.index();
    // `Off` consults no ceiling and so records no denial: a count that moved
    // under `quota=off` would report refusals on a tier that refuses nothing.
    let limit = match mode {
        QuotaMode::Off => NO_LIMIT,
        _ => row.limit[idx].load(Ordering::Acquire),
    };
    let mut packed = row.usage[idx].load(Ordering::Relaxed);
    let mut over_limit;
    loop {
        let used = usage_used(packed);
        let Some(next) = used.checked_add(n) else {
            row.denials[idx].fetch_add(1, Ordering::Relaxed);
            return Err(());
        };
        over_limit = limit != NO_LIMIT && next > limit;
        if over_limit && matches!(mode, QuotaMode::Enforce) {
            row.denials[idx].fetch_add(1, Ordering::Relaxed);
            return Err(());
        }
        let peak = usage_peak(packed).max(next);
        match row.usage[idx].compare_exchange_weak(
            packed,
            pack_usage(next, peak),
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                // Counted even though the charge was granted: `quota=warn`
                // exists to measure what enforcement *would* have refused.
                if over_limit {
                    row.denials[idx].fetch_add(1, Ordering::Relaxed);
                }
                return Ok(());
            }
            Err(observed) => packed = observed,
        }
    }
}

/// Credit one row. Saturating rather than wrapping: an under-run is a
/// bookkeeping bug, and a count wrapped to `u32::MAX` would refuse every
/// subsequent charge forever.
fn release_row(row: &AccountRow, kind: ResourceKind, n: u32) {
    let idx = kind.index();
    let mut packed = row.usage[idx].load(Ordering::Relaxed);
    loop {
        let used = usage_used(packed);
        let next = used.saturating_sub(n);
        debug_assert!(used >= n, "account row underflow on {}", kind.name());
        match row.usage[idx].compare_exchange_weak(
            packed,
            pack_usage(next, usage_peak(packed)),
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(observed) => packed = observed,
        }
    }
}

/// Give back the levels a refused walk had already debited.
fn unwind(levels: &[AccountId], kind: ResourceKind, n: u32) {
    for &id in levels {
        if let Some(row) = row_for(id) {
            release_row(row, kind, n);
        }
    }
}

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
    let usage = row.usage[idx].load(Ordering::Acquire);
    Some(KindStats {
        used: usage_used(usage),
        limit: row.limit[idx].load(Ordering::Acquire),
        peak: usage_peak(usage),
        denials: row.denials[idx].load(Ordering::Acquire),
    })
}

/// Visit every live row, lowest slot first, with its id and parent.
///
/// The root sorts first because its slot is fixed at zero, so a dump reads
/// top-down without sorting.
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
    /// An ancestor holds less than the descendants debiting through it: a
    /// refund reached a level a charge did not.
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
    /// A `Pages` row disagrees with the address spaces it accounts for.
    ///
    /// The only kind whose charge tracks a quantity that *changes* over the
    /// holder's life, so it is the only one where "the token is unique" does
    /// not already imply "the number is right": a mapping can grow, split or
    /// merge under a token that was minted once.
    ///
    /// `mapped` against `charged` catches a token that stopped tracking its own
    /// tree. `charged` against the row's `used` catches a debit that reached the
    /// account without a token behind it — the phantom-debit shape, which the
    /// other three checks are blind to because they compare rows only against
    /// each other.
    PagesMismatch {
        account: AccountId,
        /// Pages the address spaces on this account actually span.
        mapped: u32,
        /// Pages their tokens claim.
        charged: u32,
        /// What the row says, including every descendant's debit.
        used: u32,
    },
}

/// Reports each address space's `(account, mapped, charged)` page counts.
///
/// One call per bound address space, not per account: several address spaces
/// can name one account, so the audit sums the reports per account before
/// comparing. A reconciler that pre-aggregated would hide exactly the case
/// where two maps disagree in opposite directions.
pub type PagesReconciler = fn(&mut dyn FnMut(AccountId, u32, u32));

static PAGES_RECONCILER: AtomicUsize = AtomicUsize::new(0);

/// Teach the audit how to check `Pages` against the maps it accounts for.
///
/// Registered by `mm`, which owns the region trees; OSTD defines the axis but
/// cannot name a `VmaMap`. Without this the audit's other three checks pass
/// vacuously on `Pages`: a charge that drifted from its *map* is still
/// consistent with every ancestor.
pub fn register_pages_reconciler(reconciler: PagesReconciler) {
    PAGES_RECONCILER.store(reconciler as usize, Ordering::Release);
}

/// Check the ledger against itself, reporting every inconsistency to `report`.
///
/// The only mechanism that can see a forgotten or unwinder-skipped charge; each
/// check names a distinct way the numbers could be lying — see [`LedgerFault`].
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
            let usage = row.usage[idx].load(Ordering::Acquire);
            let used = usage_used(usage);
            let peak = usage_peak(usage);
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
            // is at least their sum. Saturating: several children can each
            // exceed what a `u32` sum would hold.
            let mut children = 0u32;
            for (child_slot, child) in ACCOUNTS.iter().enumerate() {
                if child_slot == slot || !child.live.load(Ordering::Acquire) {
                    continue;
                }
                if child.parent.load(Ordering::Acquire) == slot as u32 {
                    children = children
                        .saturating_add(usage_used(child.usage[idx].load(Ordering::Acquire)));
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

    let raw = PAGES_RECONCILER.load(Ordering::Acquire);
    if let Some(reconcile) =
        crate::util::fn_ptr::fn_ptr_decode_opt::<PagesReconciler>(raw as *mut ())
    {
        // One account at a time, because the address-space-to-account mapping is
        // many-to-one and a per-map comparison would report each sibling as a
        // mismatch against the shared row. Re-driving the walk per account is
        // O(accounts x maps) but pays no stack; the alternative is a 257-entry
        // accumulator, and this runs inside the 2 KiB frame cap.
        for (slot, row) in ACCOUNTS.iter().enumerate() {
            if !row.live.load(Ordering::Acquire) {
                continue;
            }
            let account = account_id_at(slot as u32);
            if account.is_none() {
                continue;
            }
            let mut mapped = 0u32;
            let mut charged = 0u32;
            let mut seen = false;
            reconcile(&mut |reported, m, c| {
                if reported != account {
                    return;
                }
                seen = true;
                mapped = mapped.saturating_add(m);
                charged = charged.saturating_add(c);
            });
            if !seen {
                continue;
            }
            // Descendants debit through this row too, so `used` is the maps'
            // own total only after their contribution is taken out.
            let idx = ResourceKind::Pages.index();
            let used = usage_used(row.usage[idx].load(Ordering::Acquire));
            let mut descendants = 0u32;
            for (child_slot, child) in ACCOUNTS.iter().enumerate() {
                if child_slot == slot || !child.live.load(Ordering::Acquire) {
                    continue;
                }
                if child.parent.load(Ordering::Acquire) == slot as u32 {
                    descendants = descendants
                        .saturating_add(usage_used(child.usage[idx].load(Ordering::Acquire)));
                }
            }
            let own = used.saturating_sub(descendants);
            if mapped != charged || own != charged {
                faults += 1;
                report(LedgerFault::PagesMismatch {
                    account,
                    mapped,
                    charged,
                    used: own,
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
/// Fabricates the inconsistency a cancel loop that missed a level produces, so
/// the audit's own test can prove the audit rejects rather than merely accepts.
#[cfg(test)]
pub(super) fn refund_raw_one_level_for_test(account: AccountId, kind: ResourceKind, n: u32) {
    // Writes the row directly rather than through `release_row`, whose underflow
    // `debug_assert` would fire on the very corruption being planted.
    if let Some(row) = row_for(account) {
        let idx = kind.index();
        let packed = row.usage[idx].load(Ordering::Acquire);
        let used = usage_used(packed).saturating_sub(n);
        row.usage[idx].store(pack_usage(used, usage_peak(packed)), Ordering::Release);
    }
}

/// Inverse of [`refund_raw_one_level_for_test`], for restoring a row a test
/// deliberately corrupted so the real token's refund still balances.
#[cfg(test)]
pub(super) fn charge_raw_one_level_for_test(account: AccountId, kind: ResourceKind, n: u32) {
    if let Some(row) = row_for(account) {
        let idx = kind.index();
        let packed = row.usage[idx].load(Ordering::Acquire);
        let used = usage_used(packed).saturating_add(n);
        row.usage[idx].store(pack_usage(used, usage_peak(packed)), Ordering::Release);
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

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as StdOrdering};
    use std::thread;

    use slopos_abi::quota::FdSlot;

    use crate::process::account::alloc_generation_for_test;
    use crate::test_support::global_lock::{GlobalTestStateGuard, lock_global_test_state};

    fn fixture() -> impl Drop {
        struct Guard(GlobalTestStateGuard);
        impl Drop for Guard {
            fn drop(&mut self) {
                reset_for_test();
            }
        }
        let guard = lock_global_test_state();
        reset_for_test();
        Guard(guard)
    }

    fn account(slot: u32, parent: AccountId) -> AccountId {
        let id = AccountId::from_parts(slot, alloc_generation_for_test());
        account_create(id, parent).expect("create");
        id
    }

    #[test]
    fn no_reader_ever_observes_used_above_peak() {
        let _f = fixture();
        let leaf = account(1, root());
        let ancestor = root();

        let stop = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let reader_started = Arc::clone(&started);
        let reader = thread::spawn(move || {
            let mut samples = 0u64;
            loop {
                for id in [ancestor, leaf] {
                    let s = stats(id, ResourceKind::FdSlot).expect("row");
                    assert!(
                        s.used <= s.peak,
                        "observed used={} above peak={} on {:?}",
                        s.used,
                        s.peak,
                        id
                    );
                }
                samples += 1;
                reader_started.store(true, StdOrdering::Release);
                if reader_stop.load(StdOrdering::Acquire) {
                    return samples;
                }
            }
        });

        while !started.load(StdOrdering::Acquire) {
            core::hint::spin_loop();
        }
        for round in 0..128u32 {
            let held = try_charge::<FdSlot>(leaf, 1 + round % 7).expect("charge");
            let also = try_charge::<FdSlot>(leaf, 2).expect("charge");
            drop(held);
            drop(also);
        }
        stop.store(true, StdOrdering::Release);

        let samples = reader.join().expect("reader");
        assert!(samples > 0, "the reader never sampled the ledger");
    }

    #[test]
    fn a_batch_refused_above_the_leaf_leaves_the_leaf_peak_raised() {
        let _f = fixture();
        let parent = account(1, root());
        let leaf = account(2, parent);
        set_limit(parent, ResourceKind::FdSlot, 2);

        try_charge::<FdSlot>(leaf, 5).expect_err("the parent ceiling refuses");

        let held = stats(leaf, ResourceKind::FdSlot).expect("row");
        assert_eq!(held.used, 0, "a refused batch must leave no debit");
        assert_eq!(held.peak, 5, "but the leaf did hold it");
        let refused = stats(parent, ResourceKind::FdSlot).expect("row");
        assert_eq!(
            refused.peak, 0,
            "the refusing row never held the amount, so its peak must not move"
        );
    }
}
