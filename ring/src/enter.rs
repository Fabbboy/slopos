//! `ring_setup` / `ring_enter` implementation (SLOPRING § 6, § 7, § 8).
//!
//! Both are **synchronous** kernel functions. `ring_enter`'s submit and
//! CQE-post bookkeeping run under the per-ring serialization lock
//! (SLOPRING § 6.3); the harvest *block* runs outside it, registering
//! the *calling task* on each in-flight fd's resource queue (the
//! poll/select shape, caller-as-waiter — SLOPRING § 7.1). The global
//! registry-table lock is held only briefly inside `registry::with_ring`
//! to clone the ring handle, never across the fileio probe or the block.

use core::ffi::c_int;

use slopos_abi::Errno;
use slopos_abi::ring::{
    OP_CANCEL, OP_NOP, OP_TIMEOUT, RingLayout, SLOPRING_ASYNC_CANCEL_ALL, SLOPRING_MAX_ENTRIES, Sqe,
};

use slopos_fs::fileio::{
    file_poll_clear_registrations, file_poll_fused, file_poll_track_registrations,
    file_poll_unfused_by_idx,
};
use slopos_kernel_services::driver_runtime::{block_current_task_with_timeout, has_pending_signal};
use slopos_kernel_services::platform::get_time_ms;
use slopos_ostd::KVec;

use crate::opcode::{self, Outcome};
use crate::region::RingRegion;
use crate::ring_obj::{Ring, heapless_vec::InFlightVec};
use crate::{file_ops, registry};

const PAGE_SIZE: u64 = 4096;
/// Cap a single harvest re-poll sleep so a wakeup we missed is bounded.
const MAX_SLEEP_MS: u32 = 50;

/// Negated-errno return value. `Errno::raw()` is *already* negative
/// (`-EAGAIN` etc.), so this returns it as-is — negating it would yield a
/// positive value that the syscall layer's `rc < 0` check and userland's
/// `res < 0` CQE check would both read as success.
fn eno(e: Errno) -> i32 {
    e.raw()
}

// ---------------------------------------------------------------------------
// ring_setup
// ---------------------------------------------------------------------------

/// `ring_setup(entries, params*)` core (SLOPRING § 6.1). Returns the
/// ring fd (`>= 0`) or a negated errno. `pid` is the caller; the
/// `out_params` closure receives the populated `RingParams` to copy to
/// the user out-pointer (so the syscall layer owns user-copy).
pub fn ring_setup(
    pid: u32,
    entries: u32,
    mut out_params: impl FnMut(&slopos_abi::ring::RingParams) -> Result<(), Errno>,
) -> i32 {
    // Validate entries: power of two in 1..=MAX.
    if entries == 0 || entries > SLOPRING_MAX_ENTRIES || !entries.is_power_of_two() {
        return eno(Errno::EINVAL);
    }

    let layout = RingLayout::new(entries);
    let n_pages = (layout.region_bytes as u64).div_ceil(PAGE_SIZE) as usize;

    // Allocate the RingMeta region.
    let region = match RingRegion::alloc(n_pages) {
        Ok(r) => r,
        Err(_) => return eno(Errno::ENOMEM),
    };

    // Write the immutable header + control masks into the region (the
    // kernel-owned indices start at 0; user indices start at 0).
    let mut params = layout.to_params();
    if write_initial_region(&region, &layout, &params).is_err() {
        return eno(Errno::EFAULT);
    }

    // Map the region into the caller's address space.
    let paddrs = region.paddrs();
    let user_addr = slopos_mm::process_vm::process_vm_map_ring(pid, paddrs.as_slice());
    if user_addr == 0 {
        return eno(Errno::ENOMEM);
    }
    params.region_addr = user_addr;
    // Re-write region_addr into the shared header so userland can read
    // it from the mapping too.
    if region.copy_in(0, &params.to_bytes()).is_err() {
        // best effort; user out-copy below is authoritative
    }

    // Build the ring object.
    let cq_cap = layout.cq_entries as usize;
    let ring = Ring {
        region,
        layout,
        sq_head: 0,
        cq_tail: 0,
        inflight: InFlightVec::with_capacity(cq_cap),
        user_addr,
        owner_pid: pid,
        cq_overflow: 0,
    };

    // Register it, get the packed fd-handle.
    let Some(raw_handle) = registry::insert(ring) else {
        // Registry full. The mapping + frames are cleaned up when the
        // process exits or unmaps; for a clean failure, unmap now.
        let _ =
            slopos_mm::process_vm::process_vm_munmap(pid, user_addr, layout.region_bytes as u64);
        return eno(Errno::ENOMEM);
    };

    // Open a FileKind::Ring fd referring to it.
    let fd = slopos_fs::fileio_open_fd_with_ops(pid, &file_ops::RING_FILE_OPS, raw_handle);
    if fd < 0 {
        registry::remove(raw_handle);
        let _ =
            slopos_mm::process_vm::process_vm_munmap(pid, user_addr, layout.region_bytes as u64);
        return fd;
    }

    // Copy the params out to the user pointer.
    if out_params(&params).is_err() {
        // Roll back: close the fd (which removes the ring) + unmap.
        let _ = slopos_fs::fileio::file_close_fd(pid, fd);
        let _ =
            slopos_mm::process_vm::process_vm_munmap(pid, user_addr, layout.region_bytes as u64);
        return eno(Errno::EFAULT);
    }

    fd
}

/// Serialize the immutable `RingParams` header + the two ring masks +
/// zeroed indices into the freshly-allocated region.
fn write_initial_region(
    region: &RingRegion,
    layout: &RingLayout,
    params: &slopos_abi::ring::RingParams,
) -> Result<(), ()> {
    // Header.
    region.copy_in(0, &params.to_bytes()).map_err(|_| ())?;
    // Masks.
    region
        .store_u32_release(layout.sq_off_mask as usize, layout.sq_entries - 1)
        .map_err(|_| ())?;
    region
        .store_u32_release(layout.cq_off_mask as usize, layout.cq_entries - 1)
        .map_err(|_| ())?;
    // Indices start at zero (region is zero-filled at alloc, but make it
    // explicit / fence-correct).
    region
        .store_u32_release(layout.sq_off_head as usize, 0)
        .map_err(|_| ())?;
    region
        .store_u32_release(layout.sq_off_tail as usize, 0)
        .map_err(|_| ())?;
    region
        .store_u32_release(layout.cq_off_head as usize, 0)
        .map_err(|_| ())?;
    region
        .store_u32_release(layout.cq_off_tail as usize, 0)
        .map_err(|_| ())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ring_enter
// ---------------------------------------------------------------------------

/// `ring_enter(ring_fd, to_submit, min_complete, flags)` core
/// (SLOPRING § 6.2). Returns the submission count (`>= 0`) or a negated
/// errno. `pid` / `task_id` identify the calling task (the waiter).
pub fn ring_enter(
    pid: u32,
    task_id: u32,
    raw_handle: usize,
    to_submit: u32,
    min_complete: u32,
    _flags: u32,
) -> i32 {
    // Reject foreign / stale rings (defence in depth over close-on-fork).
    if !registry::owner_is(raw_handle, pid) {
        return eno(Errno::EBADF);
    }

    // ---- Submit phase (under the per-ring lock only) ----
    let submit_result = registry::with_ring(raw_handle, |ring| submit(pid, ring, to_submit));
    let n_submitted = match submit_result {
        Ok(n) => n,
        Err(_) => return eno(Errno::EBADF),
    };

    // ---- Complete phase (block the caller; lock dropped while parked) ----
    if min_complete > 0 {
        let rc = harvest(pid, task_id, raw_handle, min_complete);
        // On signal with nothing submitted → EINTR; otherwise return the
        // submit count (never discard a submission — SLOPRING § 6.2).
        if rc == eno(Errno::EINTR) && n_submitted == 0 {
            return eno(Errno::EINTR);
        }
    }

    n_submitted as i32
}

/// Submit phase: consume up to `to_submit` SQEs, clamped by the SQ
/// occupancy and `sq_entries` (SLOPRING § 7, § 13.6). Returns the count
/// of SQEs consumed.
fn submit(pid: u32, ring: &mut Ring, to_submit: u32) -> u32 {
    let sq_tail = match ring.read_sq_tail() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let available = sq_tail.wrapping_sub(ring.sq_head);
    let n = to_submit.min(ring.layout.sq_entries).min(available);

    let mut consumed = 0u32;
    for _ in 0..n {
        let idx = ring.sq_head & (ring.layout.sq_entries - 1);
        let off = ring.layout.sqe_off(idx) as usize;
        let mut bytes = [0u8; 64];
        if ring.region.copy_out(off, &mut bytes).is_err() {
            break;
        }
        ring.sq_head = ring.sq_head.wrapping_add(1);
        consumed += 1;
        let sqe = Sqe::from_bytes(&bytes);
        process_sqe(pid, ring, &sqe);
    }
    let _ = ring.publish_sq_head();
    consumed
}

/// Process one submitted SQE: dispatch the special opcodes inline, run
/// the probe for the rest, and either post a CQE or record an in-flight
/// row.
fn process_sqe(pid: u32, ring: &mut Ring, sqe: &Sqe) {
    match sqe.opcode {
        OP_CANCEL => {
            do_cancel(ring, sqe);
            return;
        }
        OP_TIMEOUT => {
            // Record a timeout row: its deadline bounds the next harvest
            // block; the harvest posts -ETIME on expiry (SLOPRING § 12).
            let deadline = get_time_ms().wrapping_add(sqe.off / 1_000_000); // ns → ms
            if !ring.inflight.push(opcode::inflight_from(sqe, deadline)) {
                let _ = ring.post_cqe(sqe.user_data, eno(Errno::EAGAIN));
            }
            return;
        }
        OP_NOP => {
            let _ = ring.post_cqe(sqe.user_data, 0);
            return;
        }
        _ => {}
    }

    // Ownership ops reserve a CQE slot before running the side effect
    // (SLOPRING § 11) so a full CQ never orphans an fd / loses data.
    if opcode::is_ownership_op(sqe.opcode) {
        let cq_head = match ring.read_cq_head() {
            Ok(h) => h,
            Err(_) => {
                return;
            }
        };
        if ring.cq_full(cq_head) {
            let _ = ring.post_cqe(sqe.user_data, eno(Errno::EAGAIN));
            return;
        }
    }

    match opcode::probe(pid, sqe) {
        Outcome::Inline(res) => {
            let _ = ring.post_cqe(sqe.user_data, res);
        }
        Outcome::WouldBlock => {
            if !ring.inflight.push(opcode::inflight_from(sqe, 0)) {
                let _ = ring.post_cqe(sqe.user_data, eno(Errno::EAGAIN));
            }
        }
    }
}

/// Test hook: drive `process_sqe` against a fabricated ring without a
/// process context.
#[cfg(feature = "test-hooks")]
pub fn process_sqe_for_test(pid: u32, ring: &mut Ring, sqe: &Sqe) {
    process_sqe(pid, ring, sqe);
}

/// `OP_CANCEL`: walk the in-flight table for the target `user_data`
/// (`Sqe.addr`); remove matches and post `-ECANCELED`, then post the
/// cancel SQE's own result (SLOPRING § 10).
fn do_cancel(ring: &mut Ring, sqe: &Sqe) {
    let target = sqe.addr;
    let cancel_all = sqe.op_flags & SLOPRING_ASYNC_CANCEL_ALL != 0;
    let mut found = 0u32;
    loop {
        let Some(i) = ring.inflight.find_user_data(target) else {
            break;
        };
        if let Some(row) = ring.inflight.remove_at(i) {
            let _ = ring.post_cqe(row.user_data, eno(Errno::ECANCELED));
            found += 1;
        }
        if !cancel_all {
            break;
        }
    }
    let res = if found > 0 { 0 } else { eno(Errno::ENOENT) };
    let _ = ring.post_cqe(sqe.user_data, res);
}

/// Complete phase: block the calling task on the in-flight set until
/// `min_complete` CQEs are available, a signal arrives, or a deadline
/// elapses (SLOPRING § 7.1, § 8.3). Returns 0 on progress, or
/// `-EINTR` on signal.
fn harvest(pid: u32, task_id: u32, raw_handle: usize, min_complete: u32) -> i32 {
    loop {
        // Step 1: snapshot the distinct in-flight fds + the nearest
        // OP_TIMEOUT deadline under the lock (quick, no blocking).
        let pre = registry::with_ring(raw_handle, |ring| {
            (distinct_inflight_fds(ring), nearest_deadline_ms(ring))
        });
        let (fds, deadline) = match pre {
            Ok(v) => v,
            Err(_) => return eno(Errno::EBADF),
        };

        // Step 2: REGISTER the calling task on each fd's resource queue
        // *before* the re-probe (SLOPRING § 7.1 — register-then-recheck
        // closes the lost-wakeup window: a producer that publishes after
        // this point has already enqueued our wait node).
        let tokens = register_fds(pid, &fds);
        if !tokens.is_empty() {
            file_poll_track_registrations(task_id, tokens.as_slice());
        }

        // Step 3: re-probe every in-flight row, post ready CQEs, and
        // check whether we now have enough.
        let enough = registry::with_ring(raw_handle, |ring| harvest_step(pid, ring, min_complete));
        let enough = match enough {
            Ok(v) => v,
            Err(_) => {
                unregister(task_id, &tokens);
                return eno(Errno::EBADF);
            }
        };
        if enough {
            unregister(task_id, &tokens);
            return 0;
        }

        // Step 4: block until a wake, the re-poll cap, or a timeout
        // deadline — whichever is soonest.
        let sleep_ms = sleep_budget(deadline);
        block_current_task_with_timeout(sleep_ms);

        // Step 5: unregister symmetrically, then re-check the signal /
        // loop. (A timeout deadline elapsing is handled by the next
        // harvest_step posting -ETIME, which may then satisfy
        // min_complete.)
        unregister(task_id, &tokens);
        if has_pending_signal() {
            return eno(Errno::EINTR);
        }
    }
}

/// Unregister the calling task from every queue it registered on.
fn unregister(task_id: u32, tokens: &KVec<u64>) {
    for &tok in tokens.iter() {
        file_poll_unfused_by_idx(tok);
    }
    if !tokens.is_empty() {
        file_poll_clear_registrations(task_id);
    }
}

/// Distinct fds across all non-timeout in-flight rows (for registration).
fn distinct_inflight_fds(ring: &Ring) -> KVec<i32> {
    let mut fds: KVec<i32> = KVec::new();
    for row in ring.inflight.iter() {
        if row.opcode == OP_TIMEOUT || row.fd < 0 {
            continue;
        }
        if !fds.iter().any(|f| *f == row.fd) {
            let _ = fds.push(row.fd);
        }
    }
    fds
}

/// Nearest OP_TIMEOUT absolute deadline (ms), if any.
fn nearest_deadline_ms(ring: &Ring) -> Option<u64> {
    let mut best: Option<u64> = None;
    for row in ring.inflight.iter() {
        if row.opcode == OP_TIMEOUT && row.deadline_ms != 0 {
            best = Some(best.map_or(row.deadline_ms, |b| b.min(row.deadline_ms)));
        }
    }
    best
}

/// Compute the block timeout: the re-poll cap, clamped down to a pending
/// OP_TIMEOUT deadline if one is sooner.
fn sleep_budget(deadline: Option<u64>) -> u32 {
    match deadline {
        None => MAX_SLEEP_MS,
        Some(dl) => {
            let now = get_time_ms();
            if dl <= now {
                0
            } else {
                ((dl - now) as u32).min(MAX_SLEEP_MS)
            }
        }
    }
}

/// One harvest iteration under the ring lock: re-probe every in-flight
/// row, post ready CQEs (removing the row), honour timeout deadlines,
/// and report whether `min_complete` CQEs are now available.
fn harvest_step(pid: u32, ring: &mut Ring, min_complete: u32) -> bool {
    let now = get_time_ms();
    // Re-probe each row; collect indices to remove + CQEs to post.
    let snapshot = ring.inflight.snapshot();
    // Walk by user_data so removals don't invalidate indices.
    for row in snapshot.iter() {
        if row.opcode == OP_TIMEOUT {
            if row.deadline_ms != 0 && now >= row.deadline_ms {
                if let Some(i) = ring.inflight.find_user_data(row.user_data) {
                    if let Some(r) = ring.inflight.remove_at(i) {
                        let _ = ring.post_cqe(r.user_data, eno(Errno::ETIME));
                    }
                }
            }
            continue;
        }
        // Ownership ops (OP_ACCEPT / consuming reads) must reserve a CQE
        // slot *before* the side effect even on the deferred path — a
        // reprobe that installs an fd into a full CQ would orphan it
        // (SLOPRING § 11). If the CQ is full, leave the row in flight
        // and try again on the next harvest (after userspace drains).
        if opcode::is_ownership_op(row.opcode) {
            let cq_head = ring.read_cq_head().unwrap_or(0);
            if ring.cq_full(cq_head) {
                continue;
            }
        }
        match opcode::reprobe(pid, row) {
            Outcome::Inline(res) => {
                if let Some(i) = ring.inflight.find_user_data(row.user_data) {
                    if let Some(r) = ring.inflight.remove_at(i) {
                        let _ = ring.post_cqe(r.user_data, res);
                    }
                }
            }
            Outcome::WouldBlock => {}
        }
    }

    let cq_head = ring.read_cq_head().unwrap_or(0);
    ring.available_cqes(cq_head) >= min_complete
}

/// Register the calling task on each fd's resource queue via the
/// existing fused-poll path; return the open-file tokens for cleanup.
fn register_fds(pid: u32, fds: &KVec<i32>) -> KVec<u64> {
    use slopos_abi::syscall::{POLLIN, POLLOUT};
    let mut tokens: KVec<u64> = KVec::new();
    for &fd in fds.iter() {
        if fd < 0 {
            continue;
        }
        let r = file_poll_fused(pid, fd as c_int, POLLIN | POLLOUT);
        if r.registered {
            let _ = tokens.push(r.open_file_token);
        }
    }
    tokens
}
