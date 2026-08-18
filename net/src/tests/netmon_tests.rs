//! Tests for the network-state monitors.
//!
//! Most run two rings side by side: a ring that stops reading must not cost the
//! ring that keeps up a single record.

use slopos_fs::fileio::FdTable;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use slopos_abi::Errno;
use slopos_abi::file_ops::FileOps;
use slopos_abi::io::KernelIoBuf;
use slopos_abi::net::{
    NET_EV_ADDR_ADDED, NET_EV_DHCP, NET_EV_IFACE_CHANGED, NET_EV_NEIGH_CHANGED, NET_EV_OVERFLOW,
    NET_EV_ROUTE_ADDED, NET_EVENT_LEN, NET_MON_ADDR, NET_MON_DEFAULT, NET_MON_IFACE, NET_MON_NEIGH,
    NET_MON_ROUTE, NET_NEIGH_REACHABLE, NET_OPER_DOWN, NET_OPER_UP, NetEvent,
};
use slopos_abi::syscall::POLLIN;

use crate::netmon::{
    NETMON_MAX_PER_PROCESS, NETMON_RING_CAP, NETMON_TABLE, NetMonTable, mask_bit_for_kind,
};
use crate::netmon_file_ops::{NETMON_FILE_OPS, NetmonBacking};
use crate::netseq::net_seq;

/// A scratch registry for everything that does not specifically test the
/// kernel's own.
///
/// Deliberately **not** [`NETMON_TABLE`]: these tests run inside a live kernel,
/// and a real subscriber's ring must not be emptied by a test that wanted a
/// clean table. A `static` because the registry is ~16 KiB against a 2 KiB
/// stack-frame gate.
///
/// It shares the global sequence and wait queues with the kernel registry: the
/// sequence is what makes two monitors comparable, and a wake is re-checked by
/// whoever receives it, so a scratch post can only cause a spurious wakeup.
static TEST_MONITORS: NetMonTable = NetMonTable::new(slopos_ostd::lock_class!(
    "NETMON.test",
    slopos_ostd::sync::LOCK_LEVEL_RESOURCE
));

/// The owner these tests register monitors under.
///
/// The kernel's table rather than a synthetic pid: a monitor's owner is a
/// permission key, and a made-up number is not one any process could hold.
const TEST_OWNER: FdTable = FdTable::Kernel;

fn fresh() -> &'static NetMonTable {
    TEST_MONITORS.clear();
    &TEST_MONITORS
}

fn post_iface(table: &NetMonTable, ifindex: u32) -> u64 {
    table.post(
        NET_EV_IFACE_CHANGED,
        ifindex,
        NetEvent::iface_payload(NET_OPER_DOWN, NET_OPER_UP, 1, 1, 0, 1500),
    )
}

/// Drain everything the monitor holds, calling `f(index, record)` for each.
///
/// Chunked through a small stack array so draining a full ring stays under the
/// 2 KiB frame gate.
fn drain_each(table: &NetMonTable, handle: usize, mut f: impl FnMut(usize, NetEvent)) -> usize {
    let mut chunk = [NetEvent::default(); 8];
    let mut total = 0usize;
    loop {
        let n = match table.drain(handle, &mut chunk) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        for (i, event) in chunk[..n].iter().enumerate() {
            f(total + i, *event);
        }
        total += n;
    }
    total
}

/// How many records the monitor holds, discarding them.
fn drain_count(table: &NetMonTable, handle: usize) -> usize {
    drain_each(table, handle, |_, _| {})
}

fn test_netmon_fifo_order_and_rising_seq() -> TestResult {
    let table = fresh();
    let handle = match table.open(TEST_OWNER, NET_MON_DEFAULT) {
        Ok(h) => h,
        Err(_) => return fail!("open must succeed on an empty registry"),
    };

    for ifindex in 1..=3u32 {
        post_iface(table, ifindex);
    }

    let mut ok = true;
    let mut previous_seq = 0u64;
    let seen = drain_each(table, handle, |index, event| {
        ok &= event.ifindex == index as u32 + 1;
        ok &= event.kind == NET_EV_IFACE_CHANGED;
        ok &= event.seq > previous_seq;
        previous_seq = event.seq;
    });

    assert_eq_test!(seen, 3, "every posted record must be delivered");
    assert_test!(
        ok,
        "records must arrive in post order with rising sequences"
    );

    table.close(handle);
    pass!()
}

/// One sequence space across rings is what lets a reader merge their streams.
fn test_netmon_seq_is_global_across_rings() -> TestResult {
    let table = fresh();
    let ifaces = match table.open(TEST_OWNER, NET_MON_IFACE) {
        Ok(h) => h,
        Err(_) => return fail!("open ifaces ring"),
    };
    let addrs = match table.open(TEST_OWNER, NET_MON_ADDR) {
        Ok(h) => h,
        Err(_) => return fail!("open addrs ring"),
    };

    let first_iface = post_iface(table, 1);
    let only_addr = table.post(
        NET_EV_ADDR_ADDED,
        1,
        NetEvent::addr_payload([10, 0, 2, 15], 24, 0, 0),
    );
    let second_iface = post_iface(table, 2);

    assert_test!(
        first_iface < only_addr && only_addr < second_iface,
        "one counter numbers every event, whichever ring receives it"
    );

    let mut iface_seqs = [0u64; 2];
    let n_ifaces = drain_each(table, ifaces, |index, event| {
        if index < iface_seqs.len() {
            iface_seqs[index] = event.seq;
        }
    });
    let mut addr_seq = 0u64;
    let n_addrs = drain_each(table, addrs, |_, event| addr_seq = event.seq);

    assert_eq_test!(n_ifaces, 2, "the iface ring holds both interface records");
    assert_eq_test!(n_addrs, 1, "the addr ring holds the address record");
    assert_eq_test!(iface_seqs[0], first_iface, "sequences survive the ring");
    assert_eq_test!(addr_seq, only_addr, "sequences survive the ring");
    assert_test!(
        iface_seqs[0] < addr_seq && addr_seq < iface_seqs[1],
        "the delivered sequences interleave across the two rings"
    );

    table.close(ifaces);
    table.close(addrs);
    pass!()
}

/// The sequence advances for a change nobody is watching. A hole in the
/// numbering would be indistinguishable from a lost record, and the
/// snapshot-then-drain handoff would have no way to tell them apart.
fn test_netmon_post_without_subscribers_bumps_seq() -> TestResult {
    let table = fresh();
    assert_eq_test!(table.count(), 0, "the scratch registry starts empty");

    let before = net_seq();
    let posted = post_iface(table, 7);
    let after = net_seq();

    assert_test!(
        posted > before,
        "an unsubscribed post still claims a sequence"
    );
    assert_test!(after >= posted, "and publishes it");
    pass!()
}

/// The protocol a client uses to go from a snapshot to the live stream: open,
/// query, then discard what the snapshot already contains.
fn test_netmon_snapshot_then_drain_handoff() -> TestResult {
    let table = fresh();
    let handle = match table.open(TEST_OWNER, NET_MON_DEFAULT) {
        Ok(h) => h,
        Err(_) => return fail!("open"),
    };

    post_iface(table, 11);
    // Where a `net_query` header's `seq` would come from.
    let snapshot = net_seq();
    post_iface(table, 22);

    let mut kept = [0u32; 4];
    let mut n_kept = 0usize;
    let drained = drain_each(table, handle, |_, event| {
        if event.seq > snapshot && n_kept < kept.len() {
            kept[n_kept] = event.ifindex;
            n_kept += 1;
        }
    });

    assert_eq_test!(drained, 2, "both records reach the ring");
    assert_eq_test!(
        n_kept,
        1,
        "discarding `seq <= hdr.seq` leaves exactly what happened after the snapshot"
    );
    assert_eq_test!(kept[0], 22, "and it is the record posted after it");

    table.close(handle);
    pass!()
}

// =============================================================================
// Subscription masks
// =============================================================================

/// A monitor receives only what it asked for.
fn test_netmon_mask_filters_kinds() -> TestResult {
    let table = fresh();
    let handle = match table.open(TEST_OWNER, NET_MON_IFACE) {
        Ok(h) => h,
        Err(_) => return fail!("open"),
    };

    post_iface(table, 1);
    table.post(
        NET_EV_ADDR_ADDED,
        1,
        NetEvent::addr_payload([10, 0, 2, 15], 24, 0, 0),
    );
    table.post(
        NET_EV_ROUTE_ADDED,
        1,
        NetEvent::route_payload([0, 0, 0, 0], [10, 0, 2, 2], 0, 0, 100),
    );

    let mut kinds_ok = true;
    let seen = drain_each(table, handle, |_, event| {
        kinds_ok &= event.kind == NET_EV_IFACE_CHANGED;
    });

    assert_eq_test!(seen, 1, "only the subscribed kind is queued");
    assert_test!(kinds_ok, "and it is the interface record");

    // The same table, a ring that asked for routes instead.
    let routes = match table.open(TEST_OWNER, NET_MON_ROUTE) {
        Ok(h) => h,
        Err(_) => return fail!("open routes ring"),
    };
    post_iface(table, 2);
    table.post(
        NET_EV_ROUTE_ADDED,
        1,
        NetEvent::route_payload([0, 0, 0, 0], [10, 0, 2, 2], 0, 0, 100),
    );
    let mut route_kinds_ok = true;
    let seen_routes = drain_each(table, routes, |_, event| {
        route_kinds_ok &= event.kind == NET_EV_ROUTE_ADDED;
    });
    assert_eq_test!(seen_routes, 1, "the route ring sees only the route record");
    assert_test!(route_kinds_ok, "and nothing of the interface record");

    table.close(handle);
    table.close(routes);
    pass!()
}

/// The default subscription excludes neighbour churn. ARP is the only
/// high-rate source in the stack, and including it would keep a bounded ring in
/// permanent overflow — masking the events a subscriber opened the fd for.
fn test_netmon_default_mask_excludes_neigh() -> TestResult {
    assert_eq_test!(
        mask_bit_for_kind(NET_EV_NEIGH_CHANGED) & NET_MON_DEFAULT,
        0,
        "the neighbour bit is not in the default mask"
    );

    let table = fresh();
    let handle = match table.open(TEST_OWNER, NET_MON_DEFAULT) {
        Ok(h) => h,
        Err(_) => return fail!("open"),
    };

    table.post(
        NET_EV_NEIGH_CHANGED,
        1,
        NetEvent::neigh_payload([10, 0, 2, 2], NET_NEIGH_REACHABLE),
    );
    post_iface(table, 1);
    table.post(NET_EV_DHCP, 1, NetEvent::dhcp_payload(4, 0, 86_400));

    let mut kinds = [0u16; 4];
    let seen = drain_each(table, handle, |index, event| {
        if index < kinds.len() {
            kinds[index] = event.kind;
        }
    });

    assert_eq_test!(seen, 2, "the neighbour record is not queued by default");
    assert_eq_test!(kinds[0], NET_EV_IFACE_CHANGED, "the interface record is");
    assert_eq_test!(kinds[1], NET_EV_DHCP, "and so is the DHCP record");

    table.close(handle);
    pass!()
}

// =============================================================================
// Overflow
// =============================================================================

/// A drop episode collapses to exactly one record, ahead of what the ring kept,
/// carrying the number lost — and it is not repeated once read.
fn test_netmon_overflow_collapses_to_one_record() -> TestResult {
    const EXTRA: u32 = 6;
    let table = fresh();
    let handle = match table.open(TEST_OWNER, NET_MON_IFACE) {
        Ok(h) => h,
        Err(_) => return fail!("open"),
    };

    let mut first_seq = 0u64;
    for ifindex in 0..(NETMON_RING_CAP as u32 + EXTRA) {
        let seq = post_iface(table, ifindex);
        if ifindex == 0 {
            first_seq = seq;
        }
    }

    let mut marker = NetEvent::default();
    let mut second = NetEvent::default();
    let mut extra_markers = 0usize;
    let seen = drain_each(table, handle, |index, event| {
        match index {
            0 => marker = event,
            1 => second = event,
            _ => {}
        }
        if index > 0 && event.kind == NET_EV_OVERFLOW {
            extra_markers += 1;
        }
    });

    assert_eq_test!(
        seen,
        NETMON_RING_CAP + 1,
        "a full ring plus one overflow marker"
    );
    assert_eq_test!(marker.kind, NET_EV_OVERFLOW, "the marker comes first");
    assert_eq_test!(marker.as_u32(), EXTRA, "and carries the number dropped");
    assert_eq_test!(extra_markers, 0, "exactly one marker per episode");
    assert_eq_test!(
        second.seq,
        first_seq,
        "the retained records are the oldest — the ring drops the newest"
    );
    assert_test!(
        marker.seq > second.seq,
        "the marker names where the stream went stale, which is past what it kept"
    );

    // The latch is cleared: a quiet ring reads empty.
    assert_eq_test!(
        drain_count(table, handle),
        0,
        "the marker is not repeated on the next read"
    );

    // And a fresh drop opens a new episode.
    for ifindex in 0..(NETMON_RING_CAP as u32 + 2) {
        post_iface(table, ifindex);
    }
    let mut second_marker = NetEvent::default();
    drain_each(table, handle, |index, event| {
        if index == 0 {
            second_marker = event;
        }
    });
    assert_eq_test!(second_marker.kind, NET_EV_OVERFLOW, "a new episode marks");
    assert_eq_test!(second_marker.as_u32(), 2, "with its own count");

    table.close(handle);
    pass!()
}

/// Overflow is a property of one subscriber, not of the stream. A ring nobody
/// reads must not cost a ring that keeps up a single record — the whole reason
/// each subscriber owns its own buffer.
fn test_netmon_overflow_is_per_subscriber() -> TestResult {
    const POSTS: u32 = 100;
    let table = fresh();
    let attentive = match table.open(TEST_OWNER, NET_MON_IFACE) {
        Ok(h) => h,
        Err(_) => return fail!("open attentive ring"),
    };
    let neglected = match table.open(TEST_OWNER, NET_MON_IFACE) {
        Ok(h) => h,
        Err(_) => return fail!("open neglected ring"),
    };

    let mut attentive_seen = 0usize;
    let mut attentive_ok = true;
    for ifindex in 0..POSTS {
        let seq = post_iface(table, ifindex);
        drain_each(table, attentive, |_, event| {
            attentive_ok &= event.kind == NET_EV_IFACE_CHANGED;
            attentive_ok &= event.seq == seq;
            attentive_ok &= event.ifindex == ifindex;
            attentive_seen += 1;
        });
    }

    assert_eq_test!(
        attentive_seen,
        POSTS as usize,
        "the reader that keeps up sees every record"
    );
    assert_test!(attentive_ok, "with no gap and no marker");

    let mut marker = NetEvent::default();
    let neglected_seen = drain_each(table, neglected, |index, event| {
        if index == 0 {
            marker = event;
        }
    });

    assert_eq_test!(
        neglected_seen,
        NETMON_RING_CAP + 1,
        "the reader that never drained keeps a ring's worth plus its marker"
    );
    assert_eq_test!(marker.kind, NET_EV_OVERFLOW, "and is told it lost records");
    assert_eq_test!(
        marker.as_u32(),
        POSTS - NETMON_RING_CAP as u32,
        "as many as were dropped"
    );

    table.close(attentive);
    table.close(neglected);
    pass!()
}

/// The overflow marker reaches a subscriber whatever it subscribed to. It is
/// not a network event; it is the ring describing itself, and a reader that
/// filtered it out would silently apply a stale view forever.
fn test_netmon_overflow_ignores_mask() -> TestResult {
    assert_eq_test!(
        mask_bit_for_kind(NET_EV_OVERFLOW),
        0,
        "no subscription bit selects the marker"
    );

    let table = fresh();
    // A mask that selects nothing else this test posts.
    let handle = match table.open(TEST_OWNER, NET_MON_NEIGH) {
        Ok(h) => h,
        Err(_) => return fail!("open"),
    };

    for _ in 0..(NETMON_RING_CAP + 3) {
        table.post(
            NET_EV_NEIGH_CHANGED,
            1,
            NetEvent::neigh_payload([10, 0, 2, 2], NET_NEIGH_REACHABLE),
        );
    }
    // Filtered out, and therefore incapable of hiding the marker.
    post_iface(table, 1);

    let mut marker = NetEvent::default();
    let mut iface_records = 0usize;
    let seen = drain_each(table, handle, |index, event| {
        if index == 0 {
            marker = event;
        }
        if event.kind == NET_EV_IFACE_CHANGED {
            iface_records += 1;
        }
    });

    assert_eq_test!(seen, NETMON_RING_CAP + 1, "the ring plus its marker");
    assert_eq_test!(
        marker.kind,
        NET_EV_OVERFLOW,
        "delivered despite a mask that names no kind it belongs to"
    );
    assert_eq_test!(marker.as_u32(), 3, "carrying the count");
    assert_eq_test!(iface_records, 0, "the mask still filters real events");

    table.close(handle);
    pass!()
}

// =============================================================================
// The registry
// =============================================================================

/// The registry is quota'd, and refuses what it cannot represent. Without a
/// per-process cap, eight opens from one unprivileged process would leave
/// nothing else on the system able to watch the network.
fn test_netmon_open_is_bounded() -> TestResult {
    let table = fresh();

    assert_eq_test!(
        table.open(TEST_OWNER, 0).err(),
        Some(Errno::EINVAL),
        "an empty mask would produce an fd that can never be ready"
    );

    // A distinct *process* per pair, so the registry — not the quota — is what
    // eventually refuses. Real registrations rather than a synthetic pid plus
    // an offset: an owner is a permission key, and `FdTable`s are only
    // distinguishable if the processes behind them are.
    let mut owners: slopos_ostd::KVec<slopos_ostd::KArc<slopos_ostd::process::Process>> =
        slopos_ostd::KVec::new();
    let owner_count = slopos_abi::event::MAX_NETMON.div_ceil(NETMON_MAX_PER_PROCESS) + 1;
    for _ in 0..owner_count {
        let Ok(process) = slopos_ostd::process::process_spawn_root() else {
            return fail!("could not register a scratch process");
        };
        if owners.push(process).is_err() {
            return fail!("could not hold the scratch processes");
        }
    }
    let owner_table = |i: usize| FdTable::of(&owners[i]).expect("registered");

    let mut handles = [0usize; slopos_abi::event::MAX_NETMON];
    let mut n = 0usize;
    for slot in 0..slopos_abi::event::MAX_NETMON {
        match table.open(owner_table(slot / NETMON_MAX_PER_PROCESS), NET_MON_DEFAULT) {
            Ok(h) => {
                handles[n] = h;
                n += 1;
            }
            Err(_) => return fail!("the registry must hold MAX_NETMON monitors"),
        }
    }
    assert_eq_test!(table.count(), slopos_abi::event::MAX_NETMON, "all live");

    assert_eq_test!(
        table.open(owner_table(0), NET_MON_DEFAULT).err(),
        Some(Errno::EMFILE),
        "a process at its quota is refused before the registry is consulted"
    );
    assert_eq_test!(
        table
            .open(owner_table(owner_count - 1), NET_MON_DEFAULT)
            .err(),
        Some(Errno::ENOMEM),
        "a fresh process is refused because the registry is full"
    );

    for handle in &handles[..n] {
        table.close(*handle);
    }
    assert_eq_test!(table.count(), 0, "every slot comes back");

    // The quota counts live monitors, not opens: closing one makes room.
    let first = match table.open(TEST_OWNER, NET_MON_DEFAULT) {
        Ok(h) => h,
        Err(_) => return fail!("reopen"),
    };
    let second = match table.open(TEST_OWNER, NET_MON_DEFAULT) {
        Ok(h) => h,
        Err(_) => return fail!("reopen"),
    };
    assert_eq_test!(
        table.open(TEST_OWNER, NET_MON_DEFAULT).err(),
        Some(Errno::EMFILE),
        "two is the quota"
    );
    table.close(first);
    let third = match table.open(TEST_OWNER, NET_MON_DEFAULT) {
        Ok(h) => h,
        Err(_) => return fail!("a closed monitor must free its quota"),
    };
    table.close(second);
    table.close(third);
    pass!()
}

/// A stale handle resolves to a typed miss, never to whoever recycled the slot.
fn test_netmon_stale_handle_is_ebadf() -> TestResult {
    let table = fresh();
    let handle = match table.open(TEST_OWNER, NET_MON_DEFAULT) {
        Ok(h) => h,
        Err(_) => return fail!("open"),
    };
    post_iface(table, 1);
    table.close(handle);

    assert_eq_test!(
        table.peek(handle).err(),
        Some(Errno::EBADF),
        "a closed monitor's handle no longer reads"
    );
    assert_test!(
        table.slot_of(handle).is_none(),
        "and names no wait-queue slot"
    );

    // The recycled slot is a different monitor, and the old handle misses it.
    let recycled = match table.open(TEST_OWNER, NET_MON_DEFAULT) {
        Ok(h) => h,
        Err(_) => return fail!("reopen"),
    };
    post_iface(table, 2);
    assert_eq_test!(
        table.peek(handle).err(),
        Some(Errno::EBADF),
        "the generation makes the pre-recycle handle stale"
    );
    assert_test!(
        table.peek(recycled).ok().flatten().is_some(),
        "while the current handle reads the new monitor"
    );

    // Releasing twice is a no-op, not a second release of the recycled slot.
    table.close(handle);
    assert_eq_test!(
        table.count(),
        1,
        "a stale close cannot release the monitor that took the slot"
    );

    table.close(recycled);
    pass!()
}

/// The ring is released when its last fd closes: the backing owns the registry
/// entry, so the drop **is** the teardown.
///
/// This one runs against the kernel registry, because the backing names it.
/// It opens one monitor under a synthetic process id and gives it straight
/// back, so the registry ends exactly as it started.
fn test_netmon_ring_freed_when_backing_drops() -> TestResult {
    let before = NETMON_TABLE.count();

    let handle = match NETMON_TABLE.open(TEST_OWNER, NET_MON_DEFAULT) {
        Ok(h) => h,
        Err(_) => return fail!("the kernel registry must have a free slot"),
    };
    assert_eq_test!(NETMON_TABLE.count(), before + 1, "the monitor is live");

    let charge = slopos_ostd::process::quota::Charge::commit(
        slopos_ostd::process::quota::try_charge::<slopos_abi::quota::ObjectRow>(
            slopos_ostd::process::quota::root(),
            1,
        )
        .expect("the root account has room"),
    );
    drop(NetmonBacking {
        handle,
        object_charge: charge,
    });

    assert_eq_test!(
        NETMON_TABLE.count(),
        before,
        "dropping the fd's backing releases the ring"
    );
    assert_eq_test!(
        NETMON_TABLE.peek(handle).err(),
        Some(Errno::EBADF),
        "and the handle no longer resolves"
    );
    pass!()
}

// =============================================================================
// The fd
// =============================================================================

/// `read` delivers whole records and nothing else: a short buffer is `EINVAL`,
/// a quiet ring is `EAGAIN`, and a long buffer takes as many whole records as
/// fit. There is no partial record, so a reader needs no framing.
fn test_netmon_read_is_a_record_stride() -> TestResult {
    // Subscribed to neighbour churn, which nothing in the stack posts: this is
    // the kernel registry, so a real interface, address or route change landing
    // mid-test would be an extra record in this ring and these assertions count
    // exactly. The fd contract under test is indifferent to which kind carries
    // it.
    let handle = match NETMON_TABLE.open(TEST_OWNER, NET_MON_NEIGH) {
        Ok(h) => h,
        Err(_) => return fail!("the kernel registry must have a free slot"),
    };

    let mut short = [0u8; NET_EVENT_LEN - 1];
    let mut short_buf = KernelIoBuf::new(&mut short);
    assert_eq_test!(
        NETMON_FILE_OPS.read(handle, &mut short_buf, 0, 0),
        Errno::EINVAL.as_isize(),
        "a buffer that cannot hold one record is EINVAL"
    );

    let mut one = [0u8; NET_EVENT_LEN];
    let mut one_buf = KernelIoBuf::new(&mut one);
    assert_eq_test!(
        NETMON_FILE_OPS.read(handle, &mut one_buf, 0, 0),
        Errno::EAGAIN.as_isize(),
        "a quiet ring does not block; poll reports readiness"
    );

    let first = NETMON_TABLE.post(
        NET_EV_NEIGH_CHANGED,
        101,
        NetEvent::neigh_payload([10, 0, 2, 2], NET_NEIGH_REACHABLE),
    );
    let second = NETMON_TABLE.post(
        NET_EV_NEIGH_CHANGED,
        102,
        NetEvent::neigh_payload([10, 0, 2, 3], NET_NEIGH_REACHABLE),
    );

    // Three records of room, two records queued.
    let mut wide = [0u8; NET_EVENT_LEN * 3];
    let mut wide_buf = KernelIoBuf::new(&mut wide);
    let n = NETMON_FILE_OPS.read(handle, &mut wide_buf, 0, 0);
    assert_eq_test!(
        n,
        (NET_EVENT_LEN * 2) as isize,
        "a read takes every whole record that fits"
    );

    let mut record = [0u8; NET_EVENT_LEN];
    record.copy_from_slice(&wide[..NET_EVENT_LEN]);
    let decoded = NetEvent::from_bytes(&record);
    assert_eq_test!(decoded.seq, first, "the first record is first");
    assert_eq_test!(decoded.ifindex, 101, "and carries its interface");
    assert_eq_test!(
        decoded.as_neigh().addr,
        [10, 0, 2, 2],
        "and its typed payload"
    );

    record.copy_from_slice(&wide[NET_EVENT_LEN..NET_EVENT_LEN * 2]);
    let decoded = NetEvent::from_bytes(&record);
    assert_eq_test!(decoded.seq, second, "the second record follows");
    assert_eq_test!(decoded.ifindex, 102, "and carries its interface");

    // A one-record buffer takes exactly one, leaving the rest queued.
    NETMON_TABLE.post(
        NET_EV_NEIGH_CHANGED,
        103,
        NetEvent::neigh_payload([10, 0, 2, 4], NET_NEIGH_REACHABLE),
    );
    NETMON_TABLE.post(
        NET_EV_NEIGH_CHANGED,
        104,
        NetEvent::neigh_payload([10, 0, 2, 5], NET_NEIGH_REACHABLE),
    );
    let mut one = [0u8; NET_EVENT_LEN];
    let mut one_buf = KernelIoBuf::new(&mut one);
    assert_eq_test!(
        NETMON_FILE_OPS.read(handle, &mut one_buf, 0, 0),
        NET_EVENT_LEN as isize,
        "a one-record buffer takes one record"
    );
    let mut one_buf = KernelIoBuf::new(&mut one);
    assert_eq_test!(
        NETMON_FILE_OPS.read(handle, &mut one_buf, 0, 0),
        NET_EVENT_LEN as isize,
        "and the next read takes the one left behind"
    );

    // `write` is meaningless: the stream runs one way.
    let empty: [u8; 0] = [];
    let write_buf = slopos_abi::io::KernelIoBufRef::new(&empty);
    assert_eq_test!(
        NETMON_FILE_OPS.write(handle, &write_buf, 0, 0),
        Errno::EINVAL.as_isize(),
        "a monitor is not writable"
    );

    NETMON_TABLE.close(handle);
    let mut one_buf = KernelIoBuf::new(&mut one);
    assert_eq_test!(
        NETMON_FILE_OPS.read(handle, &mut one_buf, 0, 0),
        Errno::EBADF.as_isize(),
        "a released monitor's handle no longer reads"
    );
    pass!()
}

/// Readiness follows the ring: `POLLIN` exactly while a record is queued, and
/// `POLLNVAL` once the monitor is gone.
fn test_netmon_poll_tracks_readiness() -> TestResult {
    // A kind no production path emits — see `test_netmon_read_is_a_record_stride`.
    let handle = match NETMON_TABLE.open(TEST_OWNER, NET_MON_NEIGH) {
        Ok(h) => h,
        Err(_) => return fail!("the kernel registry must have a free slot"),
    };

    assert_eq_test!(
        NETMON_FILE_OPS.poll_events(handle, POLLIN),
        0,
        "an empty ring is not ready"
    );

    // The fused hook registers before it tests, which is what closes the
    // window a post between the test and the block would fall into.
    //
    // Only `revents` is asserted here: `registered` depends on the caller being
    // the PCR's current task, which a `net`-crate test cannot arrange — the
    // helper that installs one (`make_task_current`) lives in `core`'s test
    // support. The registration path here is the one `signalfd` uses.
    let fused = NETMON_FILE_OPS.poll_fused(handle, POLLIN);
    assert_eq_test!(fused.revents, 0, "an empty ring reports not-ready");
    NETMON_FILE_OPS.poll_unwait(handle);

    NETMON_TABLE.post(
        NET_EV_NEIGH_CHANGED,
        105,
        NetEvent::neigh_payload([10, 0, 2, 6], NET_NEIGH_REACHABLE),
    );
    assert_eq_test!(
        NETMON_FILE_OPS.poll_events(handle, POLLIN),
        POLLIN,
        "a queued record is readable"
    );
    let fused = NETMON_FILE_OPS.poll_fused(handle, POLLIN);
    assert_eq_test!(fused.revents, POLLIN, "and the fused hook agrees");
    NETMON_FILE_OPS.poll_unwait(handle);

    let mut one = [0u8; NET_EVENT_LEN];
    let mut one_buf = KernelIoBuf::new(&mut one);
    assert_eq_test!(
        NETMON_FILE_OPS.read(handle, &mut one_buf, 0, 0),
        NET_EVENT_LEN as isize,
        "drain it"
    );
    assert_eq_test!(
        NETMON_FILE_OPS.poll_events(handle, POLLIN),
        0,
        "a drained ring is quiet again"
    );

    NETMON_TABLE.close(handle);
    assert_eq_test!(
        NETMON_FILE_OPS.poll_events(handle, POLLIN),
        slopos_abi::syscall::POLLNVAL,
        "a released monitor is invalid, not merely unready"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_netmon_default_mask_excludes_neigh,
    suite = netmon
);
slopos_testing::stest!(name = test_netmon_fifo_order_and_rising_seq, suite = netmon);
slopos_testing::stest!(name = test_netmon_mask_filters_kinds, suite = netmon);
slopos_testing::stest!(name = test_netmon_open_is_bounded, suite = netmon);
slopos_testing::stest!(
    name = test_netmon_overflow_collapses_to_one_record,
    suite = netmon
);
slopos_testing::stest!(name = test_netmon_overflow_ignores_mask, suite = netmon);
slopos_testing::stest!(
    name = test_netmon_overflow_is_per_subscriber,
    suite = netmon
);
slopos_testing::stest!(name = test_netmon_poll_tracks_readiness, suite = netmon);
slopos_testing::stest!(
    name = test_netmon_post_without_subscribers_bumps_seq,
    suite = netmon
);
slopos_testing::stest!(name = test_netmon_read_is_a_record_stride, suite = netmon);
slopos_testing::stest!(
    name = test_netmon_ring_freed_when_backing_drops,
    suite = netmon
);
slopos_testing::stest!(
    name = test_netmon_seq_is_global_across_rings,
    suite = netmon
);
slopos_testing::stest!(
    name = test_netmon_snapshot_then_drain_handoff,
    suite = netmon
);
slopos_testing::stest!(name = test_netmon_stale_handle_is_ebadf, suite = netmon);
