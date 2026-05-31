//! The single-threaded reactor: it owns the one [`Ring`] and turns ring
//! completions into woken tasks.
//!
//! SLOPRING §7.1 is the load-bearing constraint: deferred completions
//! progress *only* while a task is blocked inside `ring_enter`, so the
//! reactor's sole sleep point is [`Reactor::park`] → [`Ring::submit_and_wait`].
//! On each park it drains the CQ, stores each result in its slot, and wakes
//! that slot's registered [`Waker`] — which schedules the owning task for
//! re-poll. The reactor lives in its **own** thread-local (separate from the
//! scheduler's), so `park` can fire a waker — which borrows the *scheduler's*
//! cell — without a same-cell re-borrow.
//!
//! Buffer ownership (the UAF guard): the data buffer handed to the kernel is
//! *moved into its slot* for the whole in-flight window, so it cannot be
//! freed or moved while the kernel might still write it — even if the owning
//! future is dropped mid-flight. It comes back to the future on completion,
//! or is reaped here once a cancellation lands.

use core::cell::RefCell;
use core::task::Waker;
use std::collections::HashMap;
use std::collections::VecDeque;

use slopos_abi::ring::{OP_CANCEL, OP_POLL_ADD, SLOPRING_CQE_F_MORE, SLOPRING_SQE_MULTISHOT, Sqe};
use slopos_abi::syscall::{O_NONBLOCK, POLLIN, SYSCALL_FS_CLOSE, SYSCALL_FS_READ, SYSCALL_PIPE2};
use slopos_slibc::pal::raw::{syscall1, syscall2, syscall3};

use crate::ring::{Ring, RingError};

const COOKIE_NONE: u64 = 0;
const RES_IO_ERR: i32 = -5; // -EIO
const RES_EINTR: i32 = -4; // -EINTR (transient: a signal interrupted the wait)

/// The cross-core rouse path (Phase-6 Tier B, additive). A reactor parked in
/// `ring_enter` can only be woken through an fd its ring polls; an `Rc`-based
/// `!Send` waker cannot cross threads. This self-pipe is the bridge: a
/// [`super::cross_core`] `Sender` on any thread writes one byte to `write_fd`,
/// a oneshot `OP_POLL_ADD` on `read_fd` completes in this reactor's `park`, and
/// the reactor drains the pipe, fires the local wakers in `waiters` (the
/// receiver tasks), and re-arms the poll. It is created lazily — the first
/// cross-core receiver constructed on this reactor calls
/// [`Reactor::ensure_wakeup`]; a reactor with no cross-core receiver never
/// creates it and behaves exactly as the single-threaded path always has.
///
/// A *oneshot* (re-armed each service) poll is used rather than a standing
/// multishot one: the kernel multishot poll is edge-triggered off a
/// `last_revents` cache that this reactor's out-of-band pipe drain cannot reset,
/// so a byte arriving after the drain would not re-fire. A oneshot poll probes
/// the *current* level on every (re)arm, so a post-drain byte is never lost.
struct Wakeup {
    read_fd: i32,
    write_fd: i32,
    /// Cookie of the live oneshot `OP_POLL_ADD(read_fd, POLLIN)` row, so `park`
    /// can recognise its completion. Changes on every re-arm in
    /// [`Reactor::service_wakeup`].
    poll_cookie: u64,
    /// Local wakers of tasks (cross-core receivers) parked waiting for a
    /// cross-core item. Fired on every wakeup-byte arrival; receivers re-poll
    /// and drain their queues. Spurious wakes are harmless (a re-poll that
    /// finds nothing re-registers).
    waiters: Vec<Waker>,
}

impl Drop for Wakeup {
    fn drop(&mut self) {
        // The reactor tears down (uninstall sets the cell to None) before the
        // ring is dropped; close both self-pipe ends so the fds don't leak.
        // The armed poll row is retired by the kernel when its read_fd closes.
        unsafe {
            syscall1(SYSCALL_FS_CLOSE, self.read_fd as u64);
            syscall1(SYSCALL_FS_CLOSE, self.write_fd as u64);
        }
    }
}

struct OpSlot {
    buf: Option<Vec<u8>>,
    /// FIFO of harvested completions for this op. A oneshot op gets one
    /// entry; a multishot stream can accumulate several before the
    /// consumer polls. Each entry is `(res, cqe_flags)`.
    results: VecDeque<(i32, u32)>,
    orphaned: bool,
    /// The kernel posted a terminal CQE (F_MORE clear) or the op was a
    /// oneshot whose single completion landed — no more results expected.
    terminated: bool,
    /// Waker of the task awaiting this op; fired when a completion lands.
    waker: Option<Waker>,
}

pub(super) struct Reactor {
    ring: Ring,
    slots: HashMap<u64, OpSlot>,
    next_cookie: u64,
    in_flight: usize,
    /// Cross-core rouse self-pipe. `None` until the first cross-core receiver
    /// arms it via [`Reactor::ensure_wakeup`]; a reactor that never hosts a
    /// cross-core receiver leaves this `None` and is byte-for-byte the
    /// single-threaded reactor.
    wakeup: Option<Wakeup>,
}

thread_local! {
    static REACTOR: RefCell<Option<Reactor>> = const { RefCell::new(None) };
}

/// Install a reactor over `ring` for the current thread (called by `block_on`).
pub(super) fn install(ring: Ring) {
    REACTOR.with(|c| {
        let mut b = c.borrow_mut();
        assert!(
            b.is_none(),
            "slopfut: reactor already installed (block_on is not re-entrant)"
        );
        *b = Some(Reactor {
            ring,
            slots: HashMap::new(),
            next_cookie: 1,
            in_flight: 0,
            wakeup: None,
        });
    });
}

/// Tear down the current thread's reactor (drops the ring — munmap + close).
pub(super) fn uninstall() {
    REACTOR.with(|c| {
        *c.borrow_mut() = None;
    });
}

/// Run `f` against the installed reactor. Panics if none is installed (an op
/// future was polled or dropped outside a `block_on`).
pub(super) fn with_reactor<R>(f: impl FnOnce(&mut Reactor) -> R) -> R {
    REACTOR.with(|c| {
        let mut b = c.borrow_mut();
        let r = b
            .as_mut()
            .expect("slopfut: no reactor installed — use block_on");
        f(r)
    })
}

impl Reactor {
    pub(super) fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn alloc_cookie(&mut self) -> u64 {
        let c = self.next_cookie;
        self.next_cookie = self.next_cookie.wrapping_add(1);
        if self.next_cookie == COOKIE_NONE {
            self.next_cookie = 1;
        }
        c
    }

    /// Submit one SQE, taking ownership of its (optional) data buffer. The
    /// caller has already filled `addr`/`len`; this stamps a fresh cookie and
    /// records the slot. Returns the cookie.
    pub(super) fn submit(&mut self, mut sqe: Sqe, buf: Option<Vec<u8>>) -> Result<u64, RingError> {
        let cookie = self.alloc_cookie();
        sqe.user_data = cookie;
        if self.ring.push_sqe(&sqe).is_err() {
            self.ring.submit()?;
            self.ring.push_sqe(&sqe)?;
        }
        self.ring.submit()?;
        self.slots.insert(
            cookie,
            OpSlot {
                buf,
                results: VecDeque::new(),
                orphaned: false,
                terminated: false,
                waker: None,
            },
        );
        self.in_flight += 1;
        Ok(cookie)
    }

    /// Submit an SQE armed as **multishot** (`SLOPRING_SQE_MULTISHOT`). The
    /// kernel keeps the row in flight and posts a CQE (each carrying
    /// `F_MORE`) on every yield until a terminal event; the slot persists
    /// across those CQEs and is removed only when its result queue drains
    /// after termination (see [`Reactor::take_next`]). The slot shape is
    /// identical to a oneshot's — the `F_MORE`-aware [`Reactor::park`]
    /// drain is what distinguishes the two.
    pub(super) fn submit_multishot(
        &mut self,
        mut sqe: Sqe,
        buf: Option<Vec<u8>>,
    ) -> Result<u64, RingError> {
        sqe.sqe_flags2 |= SLOPRING_SQE_MULTISHOT;
        self.submit(sqe, buf)
    }

    /// Register (or refresh) the waker to fire when `cookie` completes.
    pub(super) fn register_waker(&mut self, cookie: u64, waker: Waker) {
        if let Some(slot) = self.slots.get_mut(&cookie) {
            slot.waker = Some(waker);
        }
    }

    /// If a oneshot op's completion has landed, remove the slot and return
    /// `(res, buf)` — the buffer (untruncated) goes back to the future.
    pub(super) fn take_result(&mut self, cookie: u64) -> Option<(i32, Option<Vec<u8>>)> {
        if matches!(self.slots.get(&cookie), Some(s) if !s.results.is_empty()) {
            let mut slot = self.slots.remove(&cookie).expect("slot present");
            let (res, _flags) = slot.results.pop_front().expect("result present");
            return Some((res, slot.buf.take()));
        }
        None
    }

    /// Pop the next harvested result of a multishot stream. Returns
    /// `(res, cqe_flags, terminal)` where `terminal` is true for the final
    /// item of the stream (the kernel's terminal CQE, F_MORE clear). The
    /// slot is removed only once its result queue is empty *and* the op has
    /// terminated, so a still-armed stream keeps its slot alive between
    /// yields.
    pub(super) fn take_next(&mut self, cookie: u64) -> Option<(i32, u32, bool)> {
        let slot = self.slots.get_mut(&cookie)?;
        let (res, flags) = slot.results.pop_front()?;
        let terminal = slot.terminated && slot.results.is_empty();
        if terminal {
            // `terminated` is set only after the terminal CQE was enqueued,
            // and we only report terminal once that final result has been
            // popped — so the queue must be drained here.
            debug_assert!(
                slot.results.is_empty(),
                "slopfut: terminal reported with results still queued"
            );
            self.slots.remove(&cookie);
        }
        Some((res, flags, terminal))
    }

    /// The owning future was dropped before its op completed. Cancel the
    /// in-flight op and keep its buffer alive until the cancellation (or a
    /// late real completion) is harvested.
    pub(super) fn cancel(&mut self, cookie: u64) {
        match self.slots.get_mut(&cookie) {
            // Already terminated (oneshot completed, or a multishot whose
            // terminal CQE already landed): just drop the slot.
            Some(s) if s.terminated => {
                self.slots.remove(&cookie);
            }
            Some(s) => {
                s.orphaned = true;
                s.waker = None;
                // Discard any buffered interim results — the consumer is gone.
                s.results.clear();
                let mut sqe = Sqe::ZERO;
                sqe.opcode = OP_CANCEL;
                sqe.addr = cookie;
                sqe.user_data = COOKIE_NONE;
                let _ = self.ring.push_sqe(&sqe);
                let _ = self.ring.submit();
            }
            None => {}
        }
    }

    // --- cross-core wakeup self-pipe (Phase-6 Tier B, additive) ------------

    /// Submit a fresh **oneshot** `OP_POLL_ADD(read_fd, POLLIN)` and record its
    /// cookie as the live wakeup-poll row. A oneshot (re-armed after each
    /// service, see [`Reactor::service_wakeup`]) is used in preference to a
    /// standing multishot because the kernel multishot poll is *edge*-triggered
    /// off a `last_revents` cache the reactor's out-of-band pipe drain cannot
    /// reset — a byte arriving after the drain would not re-fire. A oneshot
    /// poll instead probes the *current* level on every (re)submit, so a byte
    /// that lands after the drain is seen by the next arm and never lost.
    fn arm_wakeup_poll(&mut self, read_fd: i32) -> Result<u64, RingError> {
        let mut sqe = Sqe::ZERO;
        sqe.opcode = OP_POLL_ADD;
        sqe.fd = read_fd;
        sqe.op_flags = POLLIN as u32;
        self.submit(sqe, None)
    }

    /// Lazily create this reactor's wakeup self-pipe and arm a oneshot
    /// `OP_POLL_ADD(read_fd, POLLIN)` over it, returning the write end a
    /// [`super::cross_core`] `Sender` writes to rouse the reactor. Idempotent:
    /// a reactor with several cross-core receivers shares one pipe (all their
    /// wakers join `waiters`). Returns `None` if the pipe or the poll arm fails
    /// (the caller falls back to a receiver that cannot be roused cross-core —
    /// degraded but not unsound on a single thread).
    pub(super) fn ensure_wakeup(&mut self) -> Option<i32> {
        if self.wakeup.is_none() {
            let mut fds = [0i32; 2];
            // O_NONBLOCK on both ends: the read end so the drain can empty to
            // EAGAIN, the write end so a full pipe (a wakeup already pending)
            // never blocks a cross-core sender.
            let rc = unsafe { syscall2(SYSCALL_PIPE2, fds.as_mut_ptr() as u64, O_NONBLOCK as u64) }
                as i64;
            if rc < 0 {
                return None;
            }
            let read_fd = fds[0];
            let write_fd = fds[1];
            let cookie = match self.arm_wakeup_poll(read_fd) {
                Ok(c) => c,
                Err(_) => {
                    unsafe {
                        syscall1(SYSCALL_FS_CLOSE, read_fd as u64);
                        syscall1(SYSCALL_FS_CLOSE, write_fd as u64);
                    }
                    return None;
                }
            };
            self.wakeup = Some(Wakeup {
                read_fd,
                write_fd,
                poll_cookie: cookie,
                waiters: Vec::new(),
            });
        }
        self.wakeup.as_ref().map(|w| w.write_fd)
    }

    /// Register `waker` to be fired on the next wakeup-byte arrival. Called by
    /// a cross-core receiver when it finds its queue empty. Deduplicated by
    /// waker identity so a receiver re-polled before the byte lands does not
    /// stack duplicate entries.
    pub(super) fn register_wakeup_waiter(&mut self, waker: Waker) {
        if let Some(w) = self.wakeup.as_mut() {
            if !w.waiters.iter().any(|x| x.will_wake(&waker)) {
                w.waiters.push(waker);
            }
        }
    }

    /// The oneshot wakeup poll fired: drain the self-pipe to EAGAIN, fire every
    /// registered waiter, and re-arm a fresh oneshot poll. Re-arming probes the
    /// pipe's current level, so any byte that arrived during/after the drain is
    /// observed by the new poll (or its first reprobe) — no lost wakeup.
    fn service_wakeup(&mut self) {
        let Some(w) = self.wakeup.as_ref() else {
            return;
        };
        let read_fd = w.read_fd;
        let mut scratch = [0u8; 64];
        loop {
            let n = unsafe {
                syscall3(
                    SYSCALL_FS_READ,
                    read_fd as u64,
                    scratch.as_mut_ptr() as u64,
                    scratch.len() as u64,
                ) as i64
            };
            if n <= 0 {
                break;
            }
        }
        // Take the waiters out before firing so a waker that re-registers
        // during its own wake does not get drained by this pass.
        let waiters: Vec<Waker> = self
            .wakeup
            .as_mut()
            .map(|w| core::mem::take(&mut w.waiters))
            .unwrap_or_default();
        for waker in waiters {
            waker.wake();
        }
        // Re-arm the oneshot poll for the next cross-core send.
        if let Ok(cookie) = self.arm_wakeup_poll(read_fd) {
            if let Some(w) = self.wakeup.as_mut() {
                w.poll_cookie = cookie;
            }
        }
    }

    /// The sole blocking point (SLOPRING §7.1): block on the ring until at
    /// least one completion lands, then drain every available CQE into its
    /// slot and wake the owning task. Live slots get their `result` set and
    /// their waker fired; orphaned slots are reaped; slot-less completions
    /// (a cancel SQE's own CQE) are discarded.
    pub(super) fn park(&mut self) {
        match self.ring.submit_and_wait(1) {
            Ok(_) => {}
            // EINTR: a signal interrupted the wait. In-flight ops untouched —
            // drain anything ready and let block_on re-park. (With the
            // signalfd model the reactor blocks the signals it harvests, so
            // this is now rare; kept as a transient-retry backstop.)
            Err(RingError::Enter(rc)) if rc == RES_EINTR => {}
            Err(_) => {
                self.fail_all();
                return;
            }
        }
        // `terminated` is set only AFTER the terminal CQE is enqueued into
        // `results`, so a slot reporting terminal always has its final
        // result already queued (no terminal-without-result race).
        let mut wakeup_fired = false;
        while let Some(cqe) = self.ring.poll_completion() {
            let more = cqe.flags & SLOPRING_CQE_F_MORE != 0;
            // The cross-core wakeup poll's CQE is not owned by any future, so it
            // bypasses the slot machinery. It is a oneshot poll, so its CQE is
            // terminal: retire its slot + in-flight count here (it is re-armed
            // with a fresh cookie in `service_wakeup`).
            // Defer the byte-drain + waiter wake until the whole CQ is drained
            // (below) so one park batches all wakeup bytes into a single pass.
            if matches!(&self.wakeup, Some(w) if w.poll_cookie == cqe.user_data) {
                self.slots.remove(&cqe.user_data);
                self.in_flight = self.in_flight.saturating_sub(1);
                wakeup_fired = true;
                continue;
            }
            match self.slots.get_mut(&cqe.user_data) {
                None => { /* cancel-SQE or stray completion: discard */ }
                Some(s) if s.orphaned => {
                    // A dropped future's op. Interim multishot CQEs are
                    // discarded; the slot is reaped only on the terminal
                    // CQE (F_MORE clear) so the in-flight accounting and
                    // the kernel row retire together.
                    if !more {
                        self.slots.remove(&cqe.user_data);
                        self.in_flight = self.in_flight.saturating_sub(1);
                    }
                }
                Some(s) => {
                    s.results.push_back((cqe.res, cqe.flags));
                    // An armed multishot row stays in flight while F_MORE is
                    // set; a oneshot op (or the terminal multishot CQE)
                    // retires it.
                    if !more {
                        s.terminated = true;
                        self.in_flight = self.in_flight.saturating_sub(1);
                    }
                    if let Some(w) = s.waker.take() {
                        w.wake();
                    }
                }
            }
        }
        if wakeup_fired {
            // One or more cross-core sends roused us: drain the self-pipe to
            // EAGAIN, wake the receiver tasks (they re-poll and drain their
            // queues), and re-arm the oneshot wakeup poll.
            self.service_wakeup();
        }
        if self.ring.cq_overflow() {
            // Unrecoverable completion loss — fail every in-flight op loud.
            self.fail_all();
        }
    }

    /// Resolve every still-in-flight op with `-EIO` and wake its task, so the
    /// owning futures complete (with an error) rather than hanging.
    fn fail_all(&mut self) {
        for slot in self.slots.values_mut() {
            if slot.results.is_empty() {
                slot.results.push_back((RES_IO_ERR, 0));
            }
            // A broken ring yields no further completions — terminate every
            // slot so even an armed multishot stream ends rather than hangs.
            slot.terminated = true;
            if let Some(w) = slot.waker.take() {
                w.wake();
            }
        }
        self.in_flight = 0;
    }
}
