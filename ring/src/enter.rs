//! `ring_setup` / `ring_enter` implementation (SLOPRING § 6, § 7, § 8).
//!
//! Submit and CQE-post bookkeeping run under the per-ring serialization lock
//! (SLOPRING § 6.3); the harvest *block* runs outside it, registering the
//! calling task on each in-flight fd's resource queue (SLOPRING § 7.1). The
//! registry-table lock is never held across the fileio probe or the block.

use slopos_abi::Errno;
use slopos_abi::ring::{
    OP_CANCEL, OP_NOP, OP_POLL_ADD, OP_TIMEOUT, RingLayout, SLOPRING_ASYNC_CANCEL_ALL,
    SLOPRING_CQE_F_MORE, SLOPRING_CQE_F_NOTIF, SLOPRING_MAX_ENTRIES, SLOPRING_SQE_FIXED_BUFFER,
    SLOPRING_SQE_MULTISHOT, Sqe,
};
use slopos_abi::syscall::{POLLERR, POLLHUP, POLLNVAL};
use slopos_fs::fileio::FdTable;

use slopos_fs::fileio::{
    FileRef, file_poll_fused_ref, file_poll_unfused_by_token, fileio_clone_file_ref,
};
use slopos_kernel_services::driver_runtime::{
    block_current_task_with_timeout, current_task_wait_aborted,
};
use slopos_kernel_services::platform::get_time_ms;
use slopos_ostd::KVec;

use crate::buffers::BufSel;
use crate::opcode::{self, Outcome};
use crate::region::RingRegion;
use crate::ring_obj::{InFlight, Ring, heapless_vec::InFlightVec};
use crate::{file_ops, registry};

const PAGE_SIZE: u64 = 4096;
/// Cap a single harvest re-poll sleep so a wakeup we missed is bounded.
const MAX_SLEEP_MS: u32 = 50;

/// Negated-errno return value. `Errno::raw()` is *already* negative, so this
/// returns it as-is — negating would yield a positive the syscall layer's
/// `rc < 0` and userland's `res < 0` CQE check both read as success.
fn eno(e: Errno) -> i32 {
    e.raw()
}

/// `ring_setup(entries, params*)` core (SLOPRING § 6.1). Returns the ring fd
/// (`>= 0`) or a negated errno; `out_params` receives the populated
/// `RingParams` so the syscall layer owns the user-copy.
pub fn ring_setup(
    table: FdTable,
    entries: u32,
    mut out_params: impl FnMut(&slopos_abi::ring::RingParams) -> Result<(), Errno>,
) -> i32 {
    if entries == 0 || entries > SLOPRING_MAX_ENTRIES || !entries.is_power_of_two() {
        return eno(Errno::EINVAL);
    }

    // A ring is mapped into a process address space; the kernel table owns none.
    let Some(vm_process) = table.process() else {
        return eno(Errno::EINVAL);
    };

    let layout = RingLayout::new(entries);
    let n_pages = (layout.region_bytes as u64).div_ceil(PAGE_SIZE) as usize;

    // Allocated first so a failure needs no rollback; boxed off `Ring`'s inline
    // body to hold the `KArc<SpinLock<Ring>>` allocation under the 2 KiB stack
    // ceiling (Inv. 5').
    let buffers = match slopos_ostd::KBox::try_new(crate::buffers::BufferRegistry::new()) {
        Ok(b) => b,
        Err(_) => return eno(Errno::ENOMEM),
    };

    let region = match RingRegion::alloc(n_pages) {
        Ok(r) => r,
        Err(_) => return eno(Errno::ENOMEM),
    };

    let mut params = layout.to_params();
    if write_initial_region(&region, &layout, &params).is_err() {
        return eno(Errno::EFAULT);
    }

    let paddrs = region.paddrs();
    let user_addr = slopos_mm::process_vm::process_vm_map_ring(vm_process, paddrs.as_slice());
    if user_addr == 0 {
        return eno(Errno::ENOMEM);
    }
    params.region_addr = user_addr;
    // Re-write region_addr into the shared header so userland can read it from
    // the mapping too.
    if region.copy_in(0, &params.to_bytes()).is_err() {
        // best effort; user out-copy below is authoritative
    }

    let cq_cap = layout.cq_entries as usize;
    // Pre-reserved to the in-flight bound so a terminal completion never
    // allocates while draining under the ring lock.
    let pending_reap = match KVec::with_capacity(cq_cap) {
        Ok(v) => v,
        Err(_) => {
            let _ = slopos_mm::process_vm::process_vm_munmap(
                vm_process,
                user_addr,
                layout.region_bytes as u64,
            );
            return eno(Errno::ENOMEM);
        }
    };
    let ring = Ring {
        region,
        layout,
        sq_head: 0,
        cq_tail: 0,
        inflight: InFlightVec::with_capacity(cq_cap),
        user_addr,
        owner: table,
        // Fixed here and never widened. Today every classified opcode is
        // permitted; the point is that the set is a property of the ring
        // rather than of the dispatch code, so narrowing it later needs no
        // new mechanism.
        allowed_ops: crate::opcode::OpcodeSet::all(),
        cq_overflow: 0,
        buffers,
        pending_reap,
    };

    let Some(raw_handle) = registry::insert(ring) else {
        let _ = slopos_mm::process_vm::process_vm_munmap(
            vm_process,
            user_addr,
            layout.region_bytes as u64,
        );
        return eno(Errno::ENOMEM);
    };

    // The backing owns the registry entry from here: a failed install drops it,
    // removing the ring, so only the mapping still needs explicit rollback.
    let Some(backing) = file_ops::ring_backing(raw_handle, table.account()) else {
        let _ = slopos_mm::process_vm::process_vm_munmap(
            vm_process,
            user_addr,
            layout.region_bytes as u64,
        );
        return eno(Errno::ENFILE);
    };
    // Process-private (SLOPRING § 14): the SQ/CQ is SPSC and the user mapping is
    // not inherited, so neither `fork` nor `exec` carries the descriptor forward.
    let fd = slopos_fs::fileio_open_fd_with_ops(
        table,
        &file_ops::RING_FILE_OPS,
        raw_handle,
        Some(backing),
        slopos_fs::FdFlags::PROCESS_PRIVATE,
    );
    if fd < 0 {
        let _ = slopos_mm::process_vm::process_vm_munmap(
            vm_process,
            user_addr,
            layout.region_bytes as u64,
        );
        return fd;
    }

    if out_params(&params).is_err() {
        let _ = slopos_fs::fileio::file_close_fd(table, fd);
        let _ = slopos_mm::process_vm::process_vm_munmap(
            vm_process,
            user_addr,
            layout.region_bytes as u64,
        );
        return eno(Errno::EFAULT);
    }

    fd
}

/// Serialize the `RingParams` header, the two ring masks and zeroed indices
/// into a freshly-allocated region.
fn write_initial_region(
    region: &RingRegion,
    layout: &RingLayout,
    params: &slopos_abi::ring::RingParams,
) -> Result<(), ()> {
    region.copy_in(0, &params.to_bytes()).map_err(|_| ())?;
    region
        .store_u32_release(layout.sq_off_mask as usize, layout.sq_entries - 1)
        .map_err(|_| ())?;
    region
        .store_u32_release(layout.cq_off_mask as usize, layout.cq_entries - 1)
        .map_err(|_| ())?;
    // Region is zero-filled at alloc; store explicitly for fence-correctness.
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
    region
        .store_u32_release(layout.cq_off_flags as usize, 0)
        .map_err(|_| ())?;
    Ok(())
}

/// `ring_enter(ring_fd, to_submit, min_complete, flags)` core (SLOPRING § 6.2).
/// Returns the submission count (`>= 0`) or a negated errno.
pub fn ring_enter(
    table: FdTable,
    raw_handle: usize,
    to_submit: u32,
    min_complete: u32,
    _flags: u32,
) -> i32 {
    // Contains a foreign or stale ring; holds for every alias of the fd,
    // including intra-process `dup`s.
    if !registry::owner_is(raw_handle, table) {
        return eno(Errno::EBADF);
    }

    let submit_result = registry::with_ring(raw_handle, |ring| {
        let n = submit(table, ring, to_submit);
        (n, core::mem::take(&mut ring.pending_reap))
    });
    let (n_submitted, reaped) = match submit_result {
        Ok(v) => v,
        Err(_) => return eno(Errno::EBADF),
    };
    // Dropped with the ring lock released: a completing op's file teardown must
    // not run under it.
    drop(reaped);

    if min_complete > 0 {
        let rc = harvest(table, raw_handle, min_complete);
        // Never discard a submission (SLOPRING § 6.2): EINTR only when nothing
        // was submitted.
        if rc == eno(Errno::EINTR) && n_submitted == 0 {
            return eno(Errno::EINTR);
        }
    }

    n_submitted as i32
}

/// Submit phase: consume up to `to_submit` SQEs, clamped by the SQ occupancy
/// and `sq_entries` (SLOPRING § 7, § 13.6).
fn submit(table: FdTable, ring: &mut Ring, to_submit: u32) -> u32 {
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
        process_sqe(table, ring, &sqe);
    }
    let _ = ring.publish_sq_head();
    consumed
}

/// Process one submitted SQE: the special opcodes inline, the rest through the
/// probe, posting a CQE or recording an in-flight row.
fn process_sqe(table: FdTable, ring: &mut Ring, sqe: &Sqe) {
    // Checked before any dispatch, so an opcode outside the ring's set never
    // reaches a probe. `EPERM` rather than `EINVAL`: the opcode is valid, this
    // ring may not submit it.
    if !ring.allowed_ops.permits(sqe.opcode) {
        let _ = ring.post_cqe(sqe.user_data, eno(Errno::EPERM), 0);
        return;
    }

    match sqe.opcode {
        OP_CANCEL => {
            do_cancel(ring, sqe);
            return;
        }
        OP_TIMEOUT => {
            // The deadline bounds the next harvest block; the harvest posts
            // -ETIME on expiry (SLOPRING § 12).
            let deadline = get_time_ms().wrapping_add(sqe.off / 1_000_000); // ns → ms
            if !ring
                .inflight
                .push(opcode::inflight_from(sqe, deadline, None))
            {
                let _ = ring.post_cqe(sqe.user_data, eno(Errno::EAGAIN), 0);
            }
            return;
        }
        OP_NOP => {
            let _ = ring.post_cqe(sqe.user_data, 0, 0);
            return;
        }
        _ => {}
    }

    // A strong reference held for the op's whole in-flight window keeps it bound
    // to this exact open file even if userland closes the fd or reuses the
    // number. A closed fd resolves to `None`; `probe` reports it as `-EBADF`.
    let file: Option<FileRef> = if opcode::needs_file_ref(sqe.opcode) {
        fileio_clone_file_ref(table, sqe.fd)
    } else {
        None
    };

    // Reserve a CQE slot before the side effect (SLOPRING § 11): a full CQ must
    // never orphan an fd or lose data.
    if opcode::is_ownership_op(sqe.opcode) {
        let cq_head = match ring.read_cq_head() {
            Ok(h) => h,
            Err(_) => {
                return;
            }
        };
        if ring.cq_full(cq_head) {
            let _ = ring.post_cqe(sqe.user_data, eno(Errno::EAGAIN), 0);
            return;
        }
    }

    let sel = opcode::buf_sel(sqe);

    // Even when ready now, a multishot row is recorded in-flight so the F_MORE
    // bookkeeping and drain loop live only in `harvest_step` (SLOPRING §1.2).
    let is_multishot =
        sqe.sqe_flags2 & SLOPRING_SQE_MULTISHOT != 0 && multishot_supported(sqe.opcode);
    if is_multishot {
        // One buffer cannot safely back an unbounded stream without per-yield
        // rotation; reject rather than silently dropping the selection.
        if sel.is_some() {
            let _ = ring.post_cqe(sqe.user_data, eno(Errno::EINVAL), 0);
            return;
        }
        if !ring.inflight.push(opcode::inflight_from(sqe, 0, file)) {
            let _ = ring.post_cqe(sqe.user_data, eno(Errno::EAGAIN), 0);
        }
        return;
    }

    // A fixed buffer stays reserved for the op's whole in-flight window so a
    // second op cannot race it and `unregister` sees it busy. Provided-ring
    // buffers reserve nothing: they are consumed when a fill actually lands.
    if let Some(BufSel::Fixed { index }) = sel
        && let Err(e) = ring.buffers.check_out_fixed(index)
    {
        let _ = ring.post_cqe(sqe.user_data, eno(e), 0);
        return;
    }

    match opcode::probe(table, sqe, file.as_ref(), &mut *ring.buffers) {
        Outcome::Inline(res) => {
            let _ = ring.post_cqe(sqe.user_data, res, 0);
            release_fixed(ring, sel);
        }
        Outcome::InlineBuf(res, cqe_flags) => {
            let _ = ring.post_cqe(sqe.user_data, res, cqe_flags);
            release_fixed(ring, sel);
        }
        Outcome::WouldBlock => {
            if !ring.inflight.push(opcode::inflight_from(sqe, 0, file)) {
                release_fixed(ring, sel);
                let _ = ring.post_cqe(sqe.user_data, eno(Errno::EAGAIN), 0);
            }
        }
        Outcome::InlineNotif(res) => {
            // The direct copy makes the registered buffer reusable the instant
            // it returns, so the F_MORE result is followed immediately by the
            // terminal F_NOTIF.
            let _ = ring.post_cqe(sqe.user_data, res, SLOPRING_CQE_F_MORE);
            let _ = ring.post_cqe(sqe.user_data, 0, SLOPRING_CQE_F_NOTIF);
            release_fixed(ring, sel);
        }
        Outcome::DeferredNotif(res) => {
            // The terminal F_NOTIF waits for the driver to reclaim the TX
            // descriptor, so the fixed buffer stays checked out across the DMA.
            let _ = ring.post_cqe(sqe.user_data, res, SLOPRING_CQE_F_MORE);
        }
    }
}

/// Release a fixed-buffer reservation if `sel` named one.
fn release_fixed(ring: &mut Ring, sel: Option<BufSel>) {
    if let Some(BufSel::Fixed { index }) = sel {
        ring.buffers.check_in_fixed(index);
    }
}

/// Release the fixed-buffer reservation a retired in-flight `row` held.
fn release_fixed_row(ring: &mut Ring, row: &InFlight) {
    if row.buf_flags & SLOPRING_SQE_FIXED_BUFFER != 0 {
        ring.buffers.check_in_fixed(row.buf_index);
    }
}

/// Opcodes that honour `SLOPRING_SQE_MULTISHOT` (SLOPRING § 1.1); any other
/// opcode ignores the flag and runs oneshot.
fn multishot_supported(opcode: u8) -> bool {
    matches!(
        opcode,
        slopos_abi::ring::OP_ACCEPT | slopos_abi::ring::OP_RECVMSG | OP_POLL_ADD
    )
}

/// Test hook: drive `process_sqe` against a fabricated ring without a
/// process context.
#[cfg(feature = "test-hooks")]
pub fn process_sqe_for_test(table: FdTable, ring: &mut Ring, sqe: &Sqe) {
    process_sqe(table, ring, sqe);
}

/// Test hook: drive one `harvest_step` pass against a fabricated ring.
#[cfg(feature = "test-hooks")]
pub fn harvest_step_for_test(table: FdTable, ring: &mut Ring, min_complete: u32) -> bool {
    harvest_step(table, ring, min_complete)
}

/// `OP_CANCEL`: remove the in-flight rows matching `Sqe.addr`, posting
/// `-ECANCELED` for each, then the cancel SQE's own result (SLOPRING § 10).
fn do_cancel(ring: &mut Ring, sqe: &Sqe) {
    let target = sqe.addr;
    let cancel_all = sqe.op_flags & SLOPRING_ASYNC_CANCEL_ALL != 0;
    let mut found = 0u32;
    loop {
        let Some(i) = ring.inflight.find_user_data(target) else {
            break;
        };
        if let Some(row) = ring.inflight.remove_at(i) {
            // Multishot rows cancel identically: one terminal -ECANCELED with
            // F_MORE clear (SLOPRING §1.3 trigger 4).
            let _ = ring.post_cqe(row.user_data, eno(Errno::ECANCELED), 0);
            release_fixed_row(ring, &row);
            reap_row(ring, row);
            found += 1;
        }
        if !cancel_all {
            break;
        }
    }
    let res = if found > 0 { 0 } else { eno(Errno::ENOENT) };
    let _ = ring.post_cqe(sqe.user_data, res, 0);
}

/// Complete phase: block the calling task on the in-flight set until
/// `min_complete` CQEs are available, a signal arrives, or a deadline elapses
/// (SLOPRING § 7.1, § 8.3). Returns 0 on progress, `-EINTR` on signal.
fn harvest(table: FdTable, raw_handle: usize, min_complete: u32) -> i32 {
    // Armed across every iteration: `register_files` below must find a live
    // token, and it must outlast `unregister`. See `PollWaiter`'s module docs.
    let waiter = slopos_ostd::sync::PollWaiter::new();
    loop {
        // The returned aliases keep the backings alive across the unlocked
        // registration below.
        let pre = registry::with_ring(raw_handle, |ring| {
            (distinct_inflight_files(ring), nearest_deadline_ms(ring))
        });
        let (files, deadline) = match pre {
            Ok(v) => v,
            Err(_) => return eno(Errno::EBADF),
        };

        // Register before the re-probe (SLOPRING § 7.1): register-then-recheck
        // closes the lost-wakeup window, since a producer publishing after this
        // point has already enqueued our wait node.
        let tokens = register_files(&files);

        let step = registry::with_ring(raw_handle, |ring| {
            let enough = harvest_step(table, ring, min_complete);
            (enough, core::mem::take(&mut ring.pending_reap))
        });
        let (enough, reaped) = match step {
            Ok(v) => v,
            Err(_) => {
                unregister(&tokens);
                return eno(Errno::EBADF);
            }
        };
        drop(reaped);
        if enough {
            unregister(&tokens);
            return 0;
        }

        let sleep_ms = sleep_budget(deadline);
        match &waiter {
            Some(waiter) => {
                waiter.block(sleep_ms);
                waiter.clear_pending();
            }
            None => block_current_task_with_timeout(sleep_ms),
        }

        unregister(&tokens);
        if current_task_wait_aborted() {
            return eno(Errno::EINTR);
        }
    }
}

fn unregister(tokens: &KVec<u64>) {
    for &tok in tokens.iter() {
        file_poll_unfused_by_token(tok);
    }
}

/// Distinct in-flight files by open-file identity, not fd number: that is what
/// closes the close+reuse aliasing window, and one file backing two rows
/// registers once. Aliased so the set outlives the ring lock.
fn distinct_inflight_files(ring: &Ring) -> KVec<FileRef> {
    let mut files: KVec<FileRef> = KVec::new();
    for row in ring.inflight.iter() {
        if row.opcode == OP_TIMEOUT {
            continue;
        }
        let Some(file) = row.file.as_ref() else {
            continue;
        };
        if !files.iter().any(|f| f.ptr_eq(file)) {
            let _ = files.push(file.alias());
        }
    }
    files
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

/// Block timeout: the re-poll cap, clamped down to a sooner OP_TIMEOUT deadline.
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

/// One harvest iteration: re-probe every in-flight row, post ready CQEs,
/// honour timeout deadlines, and report whether `min_complete` CQEs are
/// available.
///
/// Runs under the per-ring lock, so the snapshot below and every reprobe and
/// CQE post observe one atomic instant, with no concurrent submit/cancel
/// mutating the table or the indices mid-pass.
fn harvest_step(table: FdTable, ring: &mut Ring, min_complete: u32) -> bool {
    let now = get_time_ms();
    let snapshot = ring.inflight.snapshot();
    // Walk by user_data so removals don't invalidate indices.
    for row in snapshot.iter() {
        if row.opcode == OP_TIMEOUT {
            if row.deadline_ms != 0 && now >= row.deadline_ms {
                retire_row(ring, row.user_data, eno(Errno::ETIME), 0, false, false);
            }
            continue;
        }

        if row.is_multishot {
            if row.opcode == OP_POLL_ADD {
                harvest_poll_multishot(ring, row);
            } else {
                harvest_consuming_multishot(table, ring, row);
            }
            continue;
        }

        // A reprobe that installs an fd into a full CQ would orphan it
        // (SLOPRING § 11); leave the row in flight until userspace drains.
        if opcode::is_ownership_op(row.opcode) {
            let cq_head = ring.read_cq_head().unwrap_or(0);
            if ring.cq_full(cq_head) {
                continue;
            }
        }
        match opcode::reprobe(table, row, &mut *ring.buffers) {
            Outcome::Inline(res) => retire_row(ring, row.user_data, res, 0, false, true),
            Outcome::InlineBuf(res, cqe_flags) => {
                retire_row(ring, row.user_data, res, cqe_flags, false, true)
            }
            Outcome::InlineNotif(res) => {
                retire_row(ring, row.user_data, res, SLOPRING_CQE_F_MORE, true, true)
            }
            Outcome::DeferredNotif(res) => {
                // The buffer stays checked out until the reclaim posts F_NOTIF.
                retire_row(ring, row.user_data, res, SLOPRING_CQE_F_MORE, false, false)
            }
            Outcome::WouldBlock => {}
        }
    }

    // The waiter polls its own TX completion (caller-as-waiter), so a deferred
    // F_NOTIF makes progress without a TX-completion interrupt. Reclaims are
    // collected first so the `&mut buffers` borrow ends before the posts.
    if ring.buffers.has_deferred() {
        slopos_net::netdev::DEVICE_REGISTRY.poll_tx_all();
    }
    let reclaimed = ring.buffers.take_reclaimed();
    for (user_data, buf_index) in reclaimed.iter().copied() {
        let _ = ring.post_cqe(user_data, 0, SLOPRING_CQE_F_NOTIF);
        ring.buffers.check_in_fixed(buf_index);
    }

    let cq_head = ring.read_cq_head().unwrap_or(0);
    ring.available_cqes(cq_head) >= min_complete
}

/// Drain an armed consuming-multishot row (OP_ACCEPT / OP_RECVMSG) in one
/// harvest pass (SLOPRING §1.2): each reprobe posts an interim `F_MORE` CQE
/// and keeps the row armed, `WouldBlock` self-limits the drain against a CQ
/// flood, and a real error or EOF posts a terminal CQE. The ownership-op
/// CQE-slot reserve (SLOPRING § 11) is re-checked before *each* post, so a
/// full CQ leaves the row armed rather than consuming-without-a-slot.
fn harvest_consuming_multishot(table: FdTable, ring: &mut Ring, row: &InFlight) {
    let ownership = opcode::is_ownership_op(row.opcode);
    loop {
        // A concurrent cancel could have removed the row.
        if ring.inflight.find_user_data(row.user_data).is_none() {
            return;
        }
        if ownership {
            let cq_head = ring.read_cq_head().unwrap_or(0);
            if ring.cq_full(cq_head) {
                // No slot: leave armed — the reprobe has not consumed anything
                // this iteration, so nothing is lost.
                return;
            }
        }
        // A multishot row carries no registered/provided buffer and is never
        // OP_SEND_ZC, so every inline outcome collapses to one signed result.
        let res = match opcode::reprobe(table, row, &mut *ring.buffers) {
            Outcome::Inline(res)
            | Outcome::InlineBuf(res, _)
            | Outcome::InlineNotif(res)
            | Outcome::DeferredNotif(res) => res,
            Outcome::WouldBlock => {
                // Drained: leave armed, post nothing (no flood).
                return;
            }
        };
        if res >= 0 {
            // res == 0 on a stream recvmsg is orderly EOF: terminal, never
            // silently re-armed.
            if res == 0 && row.opcode == slopos_abi::ring::OP_RECVMSG {
                remove_and_post(ring, row.user_data, 0, 0);
                return;
            }
            let _ = ring.post_cqe(row.user_data, res, SLOPRING_CQE_F_MORE);
        } else {
            remove_and_post(ring, row.user_data, res, 0);
            return;
        }
    }
}

/// Re-arm an armed OP_POLL_ADD multishot row on the readiness-transition edge
/// (SLOPRING §1.2): a CQE fires only when the masked-ready bitset *changes*,
/// which suppresses the level flood caller-as-waiter would otherwise produce.
/// `POLLERR`/`POLLHUP` post one terminal CQE and retire the row.
fn harvest_poll_multishot(ring: &mut Ring, row: &InFlight) {
    let revents = opcode::probe_poll_revents(row);
    if revents & POLLNVAL != 0 {
        remove_and_post(ring, row.user_data, eno(Errno::EBADF), 0);
        return;
    }
    if revents & (POLLERR | POLLHUP) != 0 {
        let ready = revents & (opcode::poll_want(row.op_flags) | POLLERR | POLLHUP);
        remove_and_post(ring, row.user_data, ready as i32, 0);
        return;
    }
    let want = opcode::poll_want(row.op_flags);
    let ready = revents & want;
    if ready != 0 && ready != row.last_revents {
        // Recorded on the *live* row: `row` is a snapshot copy.
        ring.inflight.set_last_revents(row.user_data, ready);
        let _ = ring.post_cqe(row.user_data, ready as i32, SLOPRING_CQE_F_MORE);
    } else if ready == 0 {
        // Clear the cache so the next ready transition re-fires.
        ring.inflight.set_last_revents(row.user_data, 0);
    }
}

/// Remove the live row matching `user_data` and post its terminal CQE
/// (multishot terminals carry no fixed buffer and no notification).
fn remove_and_post(ring: &mut Ring, user_data: u64, res: i32, cqe_flags: u32) {
    retire_row(ring, user_data, res, cqe_flags, false, false);
}

/// Retire the live in-flight row `user_data`: remove it, post its terminal CQE
/// (plus a zero-copy `F_NOTIF` when `notif`), release any fixed buffer it held
/// when `release_fixed`, and move it into the reap buffer to drop off-lock.
/// `#[inline(never)]` keeps the by-value `InFlight` out of `harvest_step`'s
/// frame, holding it under the 2 KiB ceiling (Inv. 5').
#[inline(never)]
fn retire_row(
    ring: &mut Ring,
    user_data: u64,
    res: i32,
    cqe_flags: u32,
    notif: bool,
    release_fixed: bool,
) {
    if let Some(i) = ring.inflight.find_user_data(user_data) {
        if let Some(r) = ring.inflight.remove_at(i) {
            let _ = ring.post_cqe(r.user_data, res, cqe_flags);
            if notif {
                let _ = ring.post_cqe(r.user_data, 0, SLOPRING_CQE_F_NOTIF);
            }
            if release_fixed {
                release_fixed_row(ring, &r);
            }
            reap_row(ring, r);
        }
    }
}

/// Detach a retired in-flight row into the ring's reap buffer, for the caller
/// to drop once the ring lock is released.
fn reap_row(ring: &mut Ring, row: InFlight) {
    let _ = ring.pending_reap.push(row);
}

/// Register the calling task on each file's resource queue via the fused-poll
/// path; returns the open-file tokens for cleanup.
fn register_files(files: &KVec<FileRef>) -> KVec<u64> {
    use slopos_abi::syscall::{POLLIN, POLLOUT};
    let mut tokens: KVec<u64> = KVec::new();
    for file in files.iter() {
        let r = file_poll_fused_ref(file, POLLIN | POLLOUT);
        if r.registered {
            let _ = tokens.push(r.open_file_token);
        }
    }
    tokens
}
