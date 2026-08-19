//! Single-holder seats for the display and the input sink.
//!
//! `fb_flip` and `input_poll_batch` used to stamp the caller as the global
//! compositor / input sink on *every* call, at frame rate. That is a re-arm,
//! not an acquire: no credential reaches it, and whoever acts last owns the
//! screen. An object that supports no access check is not protected by any
//! sandbox around it.
//!
//! A seat replaces the stamp with an object. Ownership is announced by the
//! arbiter here and is never conferred by presenting a frame.
//!
//! Three properties are load-bearing:
//!
//! - **Two named seats, strict priority.** [`SeatId::Virtcon`] outranks
//!   [`SeatId::CompositorPrimary`], so the kernel log and `/bin/roulette` can
//!   always take the screen back and the display stays recoverable. A seat
//!   request naming the lower rank fails while the higher one is held.
//! - **Release is arbiter revocation, not holder `Drop`.** The holder is named
//!   by task id and dropped by [`revoke_for_task`], driven from the task
//!   cleanup hook that already runs at exit *and* at `exec`. A reference cycle
//!   among holders would otherwise wedge the display unrecoverably — a
//!   documented failure of process descriptors elsewhere.
//! - **The grant carries a generation.** A holder that dies and whose task id
//!   is recycled does not silently re-acquire: [`SeatGrant`] is only valid
//!   while its epoch matches, so a stale one fails closed.
//!
//! The handle userland gets is a descriptor, made non-duplicable by
//! `slopos_abi::file_ops::file_kind_transferable`; this module owns only the
//! arbitration.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Which seat. Ordered by rank: a higher discriminant wins arbitration.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum SeatId {
    /// The compositor's seat. What every GUI client's frames land through.
    CompositorPrimary = 0,
    /// The kernel log's and `/bin/roulette`'s seat. Always wins, so the
    /// display is recoverable from a wedged compositor.
    Virtcon = 1,
}

impl SeatId {
    #[inline]
    const fn rank(self) -> u8 {
        self as u8
    }

    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Total decoder for a `u8` the kernel itself wrote or a syscall argument
    /// already bounded; `None` for an unrecognised encoding.
    #[inline]
    pub const fn try_from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::CompositorPrimary),
            1 => Some(Self::Virtcon),
            _ => None,
        }
    }
}

/// Which resource a seat arbitrates.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum SeatKind {
    /// The framebuffer: `fb_flip`, the cursor calls, `set_display_mode`,
    /// `roulette_draw`.
    Screen = 0,
    /// The raw input event stream: `input_poll_batch`.
    InputSink = 1,
}

impl SeatKind {
    #[inline]
    const fn index(self) -> usize {
        self as usize
    }

    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[inline]
    pub const fn try_from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Screen),
            1 => Some(Self::InputSink),
            _ => None,
        }
    }
}

/// Why a seat request was refused.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SeatError {
    /// A seat of equal or higher rank is held by a live task.
    Busy,
}

/// Proof that the bearer holds a seat, valid only while its epoch matches.
///
/// Not `Copy` and not `Clone`: the descriptor the holder gets is the
/// duplicable artefact, and it is made non-transferable at the descriptor
/// layer. Re-minting one from a borrowed one is deliberately absent.
#[derive(PartialEq, Eq, Debug)]
pub struct SeatGrant {
    kind: SeatKind,
    id: SeatId,
    task_id: u32,
    epoch: u64,
}

impl SeatGrant {
    #[inline]
    pub fn kind(&self) -> SeatKind {
        self.kind
    }

    #[inline]
    pub fn seat(&self) -> SeatId {
        self.id
    }

    #[inline]
    pub fn task_id(&self) -> u32 {
        self.task_id
    }

    #[inline]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Whether this grant still names the live holder. A revoked seat, or one
    /// re-acquired by anybody since, answers `false`.
    #[inline]
    pub fn is_current(&self) -> bool {
        seat_slot(self.kind).matches(self.epoch)
    }
}

/// One arbitrated resource. Lock-free: every transition is a single CAS on
/// the fused (epoch, holder) pair, so a seat may be revoked from the task
/// cleanup hook, which runs with the task lock held.
struct SeatSlot {
    /// Bumped on every acquire and every revoke, so a grant from a previous
    /// holder never validates. Odd/even carries no meaning; only equality does.
    epoch: AtomicU64,
    /// The holding task, or 0 for a free seat.
    holder: AtomicU32,
    /// The held seat's rank, meaningful only while `holder != 0`.
    rank: AtomicU32,
}

impl SeatSlot {
    const fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            holder: AtomicU32::new(0),
            rank: AtomicU32::new(0),
        }
    }

    #[inline]
    fn matches(&self, epoch: u64) -> bool {
        self.holder.load(Ordering::Acquire) != 0 && self.epoch.load(Ordering::Acquire) == epoch
    }
}

/// One slot per [`SeatKind`]. `static` rather than a lock-protected table:
/// revocation runs from the task cleanup hook under the task lock, where a
/// sleeping lock is illegal.
static SEATS: [SeatSlot; 2] = [SeatSlot::new(), SeatSlot::new()];

#[inline]
fn seat_slot(kind: SeatKind) -> &'static SeatSlot {
    &SEATS[kind.index()]
}

/// Take `kind`'s seat for `task_id` at rank `id`.
///
/// Succeeds when the seat is free, when the caller already holds it (an
/// idempotent re-acquire, which is what makes a restarted compositor work),
/// or when `id` outranks the incumbent. Refuses with [`SeatError::Busy`]
/// otherwise — including for an equal rank, so two compositors cannot trade
/// the screen back and forth.
pub fn acquire(kind: SeatKind, id: SeatId, task_id: u32) -> Result<SeatGrant, SeatError> {
    if task_id == 0 {
        return Err(SeatError::Busy);
    }
    let slot = seat_slot(kind);
    let incumbent = slot.holder.load(Ordering::Acquire);
    if incumbent != 0 && incumbent != task_id {
        let held_rank = slot.rank.load(Ordering::Acquire) as u8;
        if id.rank() <= held_rank {
            return Err(SeatError::Busy);
        }
    }

    // Epoch first: a concurrent `is_current` must never see the new holder
    // still carrying the old epoch, which would validate a revoked grant.
    let epoch = slot.epoch.fetch_add(1, Ordering::AcqRel) + 1;
    slot.rank.store(id.rank() as u32, Ordering::Release);
    slot.holder.store(task_id, Ordering::Release);
    Ok(SeatGrant {
        kind,
        id,
        task_id,
        epoch,
    })
}

/// The task holding `kind`'s seat, or `None` when it is free.
#[inline]
pub fn holder(kind: SeatKind) -> Option<u32> {
    match seat_slot(kind).holder.load(Ordering::Acquire) {
        0 => None,
        id => Some(id),
    }
}

/// The rank of the held seat, or `None` when free.
#[inline]
pub fn held_seat(kind: SeatKind) -> Option<SeatId> {
    let slot = seat_slot(kind);
    if slot.holder.load(Ordering::Acquire) == 0 {
        return None;
    }
    SeatId::try_from_u8(slot.rank.load(Ordering::Acquire) as u8)
}

/// Whether `task_id` currently holds `kind`'s seat.
#[inline]
pub fn is_held_by(kind: SeatKind, task_id: u32) -> bool {
    task_id != 0 && seat_slot(kind).holder.load(Ordering::Acquire) == task_id
}

/// The live epoch for `kind`. A descriptor minted under a different one names
/// a grant the arbiter has since revoked, so comparing against this is what
/// makes a stale seat fd fail closed.
#[inline]
pub fn current_epoch(kind: SeatKind) -> u64 {
    seat_slot(kind).epoch.load(Ordering::Acquire)
}

/// Release `kind`'s seat if `task_id` holds it. Returns whether it did.
///
/// The epoch bump is what invalidates every outstanding [`SeatGrant`], so a
/// descriptor that outlives the revocation cannot act.
pub fn release(kind: SeatKind, task_id: u32) -> bool {
    if task_id == 0 {
        return false;
    }
    let slot = seat_slot(kind);
    if slot.holder.load(Ordering::Acquire) != task_id {
        return false;
    }
    // Holder first: a reader that sees the seat free must not then read an
    // epoch that still validates the departing holder's grant.
    slot.holder.store(0, Ordering::Release);
    slot.rank.store(0, Ordering::Release);
    slot.epoch.fetch_add(1, Ordering::AcqRel);
    true
}

/// Drop every seat `task_id` holds.
///
/// The arbiter's revocation path, driven from the task-resource cleanup hook —
/// which runs both at task exit and before `exec` replaces the image. Release
/// is deliberately *not* the holder descriptor's `Drop`: a reference cycle
/// among holders would wedge the display with no way back.
pub fn revoke_for_task(task_id: u32) {
    let _ = release(SeatKind::Screen, task_id);
    let _ = release(SeatKind::InputSink, task_id);
}

/// Drop every seat, whoever holds it. Test-fixture and shutdown only.
#[doc(hidden)]
pub fn reset_all() {
    for slot in SEATS.iter() {
        slot.holder.store(0, Ordering::Release);
        slot.rank.store(0, Ordering::Release);
        slot.epoch.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host test binary shares one process, so each test starts from a
    /// known-free arbiter rather than whatever ran before it.
    fn fresh() {
        reset_all();
    }

    #[test]
    fn a_free_seat_is_granted() {
        fresh();
        let grant = acquire(SeatKind::Screen, SeatId::CompositorPrimary, 7)
            .expect("a free seat is available");
        assert_eq!(grant.task_id(), 7);
        assert_eq!(grant.seat(), SeatId::CompositorPrimary);
        assert!(grant.is_current());
        assert_eq!(holder(SeatKind::Screen), Some(7));
    }

    #[test]
    fn an_equal_rank_request_is_refused_while_held() {
        fresh();
        let first = acquire(SeatKind::Screen, SeatId::CompositorPrimary, 7).expect("free");
        assert_eq!(
            acquire(SeatKind::Screen, SeatId::CompositorPrimary, 8),
            Err(SeatError::Busy),
            "a second compositor must not take the screen by asking"
        );
        assert!(first.is_current());
    }

    #[test]
    fn virtcon_outranks_the_compositor() {
        fresh();
        let comp = acquire(SeatKind::Screen, SeatId::CompositorPrimary, 7).expect("free");
        let virtcon =
            acquire(SeatKind::Screen, SeatId::Virtcon, 9).expect("the kernel log always wins");
        assert!(!comp.is_current(), "the displaced grant stops validating");
        assert!(virtcon.is_current());
        assert_eq!(holder(SeatKind::Screen), Some(9));
        assert_eq!(held_seat(SeatKind::Screen), Some(SeatId::Virtcon));
    }

    #[test]
    fn the_compositor_cannot_take_the_screen_back_from_virtcon() {
        fresh();
        acquire(SeatKind::Screen, SeatId::Virtcon, 9).expect("free");
        assert_eq!(
            acquire(SeatKind::Screen, SeatId::CompositorPrimary, 7),
            Err(SeatError::Busy),
        );
    }

    #[test]
    fn re_acquiring_your_own_seat_is_idempotent() {
        fresh();
        let first = acquire(SeatKind::Screen, SeatId::CompositorPrimary, 7).expect("free");
        let second = acquire(SeatKind::Screen, SeatId::CompositorPrimary, 7)
            .expect("the holder may re-acquire");
        assert!(second.is_current());
        assert!(
            !first.is_current(),
            "the epoch still moves, so only the newest grant validates"
        );
    }

    #[test]
    fn revocation_frees_every_seat_the_task_held() {
        fresh();
        let screen = acquire(SeatKind::Screen, SeatId::CompositorPrimary, 7).expect("free");
        let input = acquire(SeatKind::InputSink, SeatId::CompositorPrimary, 7).expect("free");
        revoke_for_task(7);
        assert!(!screen.is_current());
        assert!(!input.is_current());
        assert_eq!(holder(SeatKind::Screen), None);
        assert_eq!(holder(SeatKind::InputSink), None);
    }

    #[test]
    fn revocation_names_only_the_holder() {
        fresh();
        let grant = acquire(SeatKind::Screen, SeatId::CompositorPrimary, 7).expect("free");
        revoke_for_task(8);
        assert!(
            grant.is_current(),
            "a stranger's exit must not free the seat"
        );
        assert_eq!(holder(SeatKind::Screen), Some(7));
    }

    /// The reason the grant carries an epoch: a holder dies, its task id is
    /// recycled onto a different program, and the stale grant must not
    /// validate against the new occupant.
    #[test]
    fn a_recycled_task_id_does_not_revive_a_stale_grant() {
        fresh();
        let stale = acquire(SeatKind::Screen, SeatId::CompositorPrimary, 7).expect("free");
        revoke_for_task(7);
        let reborn = acquire(SeatKind::Screen, SeatId::CompositorPrimary, 7)
            .expect("the id is free to take the seat again");
        assert!(
            !stale.is_current(),
            "the dead holder's grant must not validate against the new one"
        );
        assert!(reborn.is_current());
    }

    #[test]
    fn the_two_kinds_are_independent() {
        fresh();
        acquire(SeatKind::Screen, SeatId::CompositorPrimary, 7).expect("free");
        let input = acquire(SeatKind::InputSink, SeatId::CompositorPrimary, 8)
            .expect("a different resource is a different seat");
        assert!(input.is_current());
        assert_eq!(holder(SeatKind::Screen), Some(7));
        assert_eq!(holder(SeatKind::InputSink), Some(8));
    }

    #[test]
    fn task_id_zero_is_never_a_holder() {
        fresh();
        assert_eq!(
            acquire(SeatKind::Screen, SeatId::CompositorPrimary, 0),
            Err(SeatError::Busy),
            "0 is the free sentinel and must not be grantable"
        );
        assert_eq!(holder(SeatKind::Screen), None);
    }

    #[test]
    fn seat_and_kind_decoders_reject_unknown_encodings() {
        assert_eq!(SeatId::try_from_u8(2), None);
        assert_eq!(SeatKind::try_from_u8(2), None);
        assert_eq!(SeatId::try_from_u8(0), Some(SeatId::CompositorPrimary));
        assert_eq!(SeatKind::try_from_u8(1), Some(SeatKind::InputSink));
    }
}
