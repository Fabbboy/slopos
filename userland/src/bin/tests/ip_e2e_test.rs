#![feature(restricted_std)]

//! End-to-end proof of the network management surface against the live stack
//! in QEMU, from the syscalls up to what `/bin/ip` actually renders.
//!
//! The utest runner spawns test binaries with `TASK_FLAG_USER_MODE |
//! TASK_FLAG_SYSTEM`, so this caller holds `SYSTEM` and does not hold
//! `NET_ADMIN`: `require_net_admin` tests one flag that only the kernel's
//! program-identity table confers, on `/bin/ip`. Every mutation below therefore
//! goes *through* `/bin/ip` rather than being issued directly.
//!
//! Records are decoded by explicit offset — the ones `abi/src/net.rs` pins with
//! `offset_of!` — rather than through `apps::ip::query`, so a layout bug in the
//! renderer's walker cannot make these assertions pass.

// Links the lib crate's `_start` ELF entry point into the binary; without it
// the linker emits entry 0x0 and `do_exec` rejects the ELF.
use slopos_userland as _;

use std::string::String;
use std::vec::Vec;

use slopos_abi::net::{
    AF_INET, NET_EVENT_LEN, NET_IFINDEX_NONE, NET_IFKIND_ETHERNET, NET_IFKIND_LOOPBACK,
    NET_IFOP_SET_ADMIN_UP, NET_MON_DEFAULT, NET_OPER_UNKNOWN, NET_Q_GLOBAL, NET_Q_IFACES,
    NET_Q_ROUTES, NET_Q_SOCKETS, NET_SOCK_ESTABLISHED, NetEvent, SOCK_DGRAM, UserSockInfo,
};
use slopos_abi::syscall::{POLLIN, UserPollFd};
use slopos_abi::task::{INVALID_PROCESS_ID, TASK_FLAG_USER_MODE, TaskPriority};
use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::fs;
use slopos_userland::syscall::net::{net_iface_ctl, net_monitor, net_query};
use slopos_userland::syscall::process;

/// `size_of::<UserNetQueryHdr>()`, and the smallest buffer the query accepts.
const HDR: usize = 24;
/// `size_of::<UserIface>()`.
const IFACE_SIZE: usize = 104;

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

struct Hdr {
    record_size: u32,
    record_count: u32,
    total_count: u32,
    what: u32,
}

fn decode_hdr(bytes: &[u8]) -> Hdr {
    Hdr {
        record_size: u32_at(bytes, 8),
        record_count: u32_at(bytes, 12),
        total_count: u32_at(bytes, 16),
        what: u32_at(bytes, 20),
    }
}

struct Iface {
    ifindex: u32,
    kind: u8,
    oper_state: u8,
    admin_up: u8,
    name: String,
}

fn decode_iface(bytes: &[u8]) -> Iface {
    let name_bytes = &bytes[16..32];
    let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(16);
    Iface {
        ifindex: u32_at(bytes, 0),
        kind: bytes[12],
        oper_state: bytes[13],
        admin_up: bytes[15],
        name: String::from_utf8_lossy(&name_bytes[..end]).into_owned(),
    }
}

/// Run the two-call sizing protocol and decode every interface that came back.
fn read_ifaces() -> Result<(Hdr, Vec<Iface>), SyscallError> {
    let mut probe = [0u8; HDR];
    net_query(NET_Q_IFACES, NET_IFINDEX_NONE, &mut probe)?;
    let sizing = decode_hdr(&probe);

    let stride = (sizing.record_size as usize).max(1);
    let mut buf = std::vec![0u8; HDR + sizing.total_count as usize * stride];
    net_query(NET_Q_IFACES, NET_IFINDEX_NONE, &mut buf)?;

    let hdr = decode_hdr(&buf);
    let stride = (hdr.record_size as usize).max(1);
    let mut rows = Vec::new();
    for i in 0..hdr.record_count as usize {
        let start = HDR + i * stride;
        rows.push(decode_iface(&buf[start..start + stride]));
    }
    Ok((hdr, rows))
}

fn find<'a>(rows: &'a [Iface], name: &str) -> Option<&'a Iface> {
    rows.iter().find(|row| row.name == name)
}

/// How many records `what` reports for `ifindex`. The count is the whole
/// answer for the filter cases below, so only the header is read.
fn count_for(what: u32, ifindex: u32) -> Result<u32, SyscallError> {
    let mut probe = [0u8; HDR];
    net_query(what, ifindex, &mut probe)?;
    Ok(decode_hdr(&probe).total_count)
}

/// Spawn `path` with `words` as its argv, sending stdout to `stdout_fd`.
///
/// Deliberately nothing but `TASK_FLAG_USER_MODE`: `NET_ADMIN` comes from the
/// program-identity table keyed on the path, not from what this caller holds.
fn spawn_prog(path: &[u8], words: &[&str], stdout_fd: i32) -> i32 {
    let owned: Vec<Vec<u8>> = words
        .iter()
        .map(|w| {
            let mut s = Vec::with_capacity(w.len() + 1);
            s.extend_from_slice(w.as_bytes());
            s.push(0);
            s
        })
        .collect();
    // `spawn_path` reads exactly `argv.len()` pointers, so the array carries no
    // trailing NULL; each string is NUL-terminated instead.
    let argv: Vec<*const u8> = owned.iter().map(|s| s.as_ptr()).collect();
    let actions = [
        process::clone_fd(0, 0),
        process::clone_fd(stdout_fd, 1),
        process::clone_fd(2, 2),
    ];
    let tid = process::spawn_path_with_actions(
        path,
        &argv,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
    );
    drop(owned);
    tid
}

fn run_ip(words: &[&str]) -> i32 {
    let tid = spawn_prog(b"/bin/ip", words, 1);
    if tid <= 0 {
        return tid;
    }
    process::waitpid(tid as u32)
}

/// Run `path` and collect what it wrote to stdout.
///
/// The parent's write end is closed immediately after the spawn, or the read
/// never sees EOF. The pipe is drained *before* reaping, so output larger than
/// one pipe buffer cannot deadlock the pair.
fn capture_prog(path: &[u8], words: &[&str]) -> Option<String> {
    let (read_end, write_end) = fs::pipe().ok()?;
    let tid = spawn_prog(path, words, write_end.raw());
    drop(write_end);
    if tid <= 0 {
        eprintln!("ip_e2e_test: spawn of {words:?} returned {tid}");
        return None;
    }

    let mut out = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match fs::read_slice(read_end.raw(), &mut chunk) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(err) => {
                eprintln!("ip_e2e_test: read from spawn pipe failed: {}", err.errno());
                break;
            }
        }
    }
    process::waitpid(tid as u32);
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn capture_ip(words: &[&str]) -> Option<String> {
    capture_prog(b"/bin/ip", words)
}

fn dump(label: &str, text: &str) {
    eprintln!("ip_e2e_test: ---- {label} ----");
    for line in text.lines() {
        eprintln!("ip_e2e_test: | {line}");
    }
    eprintln!("ip_e2e_test: ---- end {label} ----");
}

fn iface_table_has_lo_and_ethernet() -> bool {
    let (hdr, rows) = match read_ifaces() {
        Ok(v) => v,
        Err(err) => {
            eprintln!("ip_e2e_test: net_query(IFACES) failed: {}", err.errno());
            return false;
        }
    };

    if hdr.what != NET_Q_IFACES {
        eprintln!(
            "ip_e2e_test: header echoed what={}, want {NET_Q_IFACES}",
            hdr.what
        );
        return false;
    }
    if hdr.record_size as usize != IFACE_SIZE {
        eprintln!(
            "ip_e2e_test: record_size={}, want {IFACE_SIZE}",
            hdr.record_size
        );
        return false;
    }
    if rows.len() < 2 {
        eprintln!("ip_e2e_test: only {} interface(s), want >= 2", rows.len());
        return false;
    }

    let Some(lo) = find(&rows, "lo") else {
        eprintln!("ip_e2e_test: no `lo` in the interface table");
        return false;
    };
    if lo.kind != NET_IFKIND_LOOPBACK {
        eprintln!("ip_e2e_test: lo reports kind {}, want loopback", lo.kind);
        return false;
    }
    if !rows.iter().any(|row| row.kind == NET_IFKIND_ETHERNET) {
        eprintln!("ip_e2e_test: no Ethernet interface in the table");
        return false;
    }
    true
}

/// RFC 2863 has no operational state for a link layer that does not exist, so
/// `UNKNOWN` rather than `UP`; `ip link` on Linux prints `state UNKNOWN` for
/// `lo` for the same reason.
fn loopback_operstate_is_unknown() -> bool {
    let Ok((_, rows)) = read_ifaces() else {
        eprintln!("ip_e2e_test: net_query(IFACES) failed");
        return false;
    };
    let Some(lo) = find(&rows, "lo") else {
        eprintln!("ip_e2e_test: no `lo` in the interface table");
        return false;
    };
    if lo.oper_state != NET_OPER_UNKNOWN {
        eprintln!(
            "ip_e2e_test: lo oper_state={}, want UNKNOWN ({NET_OPER_UNKNOWN})",
            lo.oper_state
        );
        return false;
    }
    if lo.admin_up == 0 {
        eprintln!("ip_e2e_test: lo is administratively down; UNKNOWN would be trivial");
        return false;
    }
    true
}

/// The truncation contract: how much exists is read from the header, never from
/// the return value.
///
/// `SYSCALL_NET_QUERY`'s doc says "a zero-length buffer is the sizing query",
/// but `write_header` refuses anything shorter than the header with `EINVAL`,
/// so the smallest buffer that actually sizes is exactly one header. Both are
/// pinned here so the discrepancy stays visible.
fn truncation_is_reported_in_the_header() -> bool {
    let mut nothing: [u8; 0] = [];
    match net_query(NET_Q_IFACES, NET_IFINDEX_NONE, &mut nothing) {
        Err(err) if err == SyscallError::EINVAL => {}
        Err(err) => {
            eprintln!(
                "ip_e2e_test: zero-length query returned errno {}, want EINVAL",
                err.errno()
            );
            return false;
        }
        Ok(n) => {
            eprintln!("ip_e2e_test: zero-length query wrote {n} bytes, want EINVAL");
            return false;
        }
    }

    let mut probe = [0u8; HDR];
    let written = match net_query(NET_Q_IFACES, NET_IFINDEX_NONE, &mut probe) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("ip_e2e_test: sizing query failed: {}", err.errno());
            return false;
        }
    };
    let sizing = decode_hdr(&probe);
    if written != HDR {
        eprintln!("ip_e2e_test: sizing query wrote {written} bytes, want {HDR}");
        return false;
    }
    if sizing.record_count != 0 {
        eprintln!(
            "ip_e2e_test: sizing query transferred {} record(s), want 0",
            sizing.record_count
        );
        return false;
    }
    let total = sizing.total_count;
    if total < 2 {
        eprintln!("ip_e2e_test: sizing query reports {total} interfaces, want >= 2");
        return false;
    }

    let mut one = std::vec![0u8; HDR + IFACE_SIZE];
    let written = match net_query(NET_Q_IFACES, NET_IFINDEX_NONE, &mut one) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("ip_e2e_test: one-record query failed: {}", err.errno());
            return false;
        }
    };
    let hdr = decode_hdr(&one);
    if hdr.record_count != 1 {
        eprintln!(
            "ip_e2e_test: one-record query transferred {}, want 1",
            hdr.record_count
        );
        return false;
    }
    if hdr.total_count != total {
        eprintln!(
            "ip_e2e_test: one-record query reports total {}, want {total}",
            hdr.total_count
        );
        return false;
    }
    if written != HDR + IFACE_SIZE {
        eprintln!(
            "ip_e2e_test: one-record query wrote {written} bytes, want {}",
            HDR + IFACE_SIZE
        );
        return false;
    }
    true
}

/// The read/write split of this ABI: anyone may ask what the network looks like
/// and subscribe to changes; only `/bin/ip` may change it.
fn queries_and_monitor_are_unprivileged() -> bool {
    let mut probe = [0u8; HDR];
    if let Err(err) = net_query(NET_Q_IFACES, NET_IFINDEX_NONE, &mut probe) {
        eprintln!(
            "ip_e2e_test: unprivileged NET_Q_IFACES failed: {}",
            err.errno()
        );
        return false;
    }
    if let Err(err) = net_query(NET_Q_GLOBAL, NET_IFINDEX_NONE, &mut probe) {
        eprintln!(
            "ip_e2e_test: unprivileged NET_Q_GLOBAL failed: {}",
            err.errno()
        );
        return false;
    }
    match net_monitor(NET_MON_DEFAULT, 0) {
        Ok(fd) => drop(fd),
        Err(err) => {
            eprintln!(
                "ip_e2e_test: unprivileged net_monitor failed: {}",
                err.errno()
            );
            return false;
        }
    }
    true
}

/// A mutating `net_iface_ctl` issued directly from here is `EPERM` even holding
/// `TASK_FLAG_SYSTEM`. The control is the case below, which performs the same
/// operation through `/bin/ip` and has it succeed.
fn direct_mutation_is_eperm_despite_system() -> bool {
    let Ok((_, rows)) = read_ifaces() else {
        eprintln!("ip_e2e_test: net_query(IFACES) failed");
        return false;
    };
    let Some(lo) = find(&rows, "lo") else {
        eprintln!("ip_e2e_test: no `lo` in the interface table");
        return false;
    };

    match net_iface_ctl(lo.ifindex, NET_IFOP_SET_ADMIN_UP, 0) {
        Err(err) if err == SyscallError::EPERM => {}
        Err(err) => {
            eprintln!(
                "ip_e2e_test: direct SET_ADMIN_UP returned errno {}, want EPERM",
                err.errno()
            );
            return false;
        }
        Ok(()) => {
            eprintln!("ip_e2e_test: direct SET_ADMIN_UP SUCCEEDED — containment is broken");
            // Put it back before failing: a test that breaks the stack on its
            // way out makes every later case unreadable.
            let _ = net_iface_ctl(lo.ifindex, NET_IFOP_SET_ADMIN_UP, 1);
            return false;
        }
    }

    let Ok((_, after)) = read_ifaces() else {
        return false;
    };
    match find(&after, "lo") {
        Some(lo_after) if lo_after.admin_up != 0 => true,
        Some(_) => {
            eprintln!("ip_e2e_test: lo went down despite the EPERM");
            false
        }
        None => {
            eprintln!("ip_e2e_test: lo vanished");
            false
        }
    }
}

/// Every `NET_Q_*` takes an ifindex, and accepting it while enumerating
/// everything anyway is worse than rejecting it: the caller gets a plausible
/// answer to a different question. The renderer passes the index straight
/// through and cannot tell, so the assertion belongs here.
fn queries_honour_their_ifindex_filter() -> bool {
    let Ok((_, rows)) = read_ifaces() else {
        eprintln!("ip_e2e_test: net_query(IFACES) failed");
        return false;
    };
    let Some(lo) = find(&rows, "lo") else {
        eprintln!("ip_e2e_test: no `lo` in the interface table");
        return false;
    };
    let total = rows.len() as u32;
    if total < 2 {
        eprintln!("ip_e2e_test: need >= 2 interfaces to tell a filter from a no-op");
        return false;
    }

    let mut buf = std::vec![0u8; HDR + total as usize * IFACE_SIZE];
    if let Err(err) = net_query(NET_Q_IFACES, lo.ifindex, &mut buf) {
        eprintln!("ip_e2e_test: scoped IFACES query failed: {}", err.errno());
        return false;
    }
    let hdr = decode_hdr(&buf);
    if hdr.total_count != 1 || hdr.record_count != 1 {
        eprintln!(
            "ip_e2e_test: IFACES scoped to lo returned {} of {total}, want 1",
            hdr.total_count
        );
        return false;
    }
    let stride = (hdr.record_size as usize).max(1);
    let only = decode_iface(&buf[HDR..HDR + stride]);
    if only.name != "lo" {
        eprintln!("ip_e2e_test: IFACES scoped to lo returned `{}`", only.name);
        return false;
    }

    let all_routes = match count_for(NET_Q_ROUTES, NET_IFINDEX_NONE) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("ip_e2e_test: unscoped ROUTES query failed: {}", err.errno());
            return false;
        }
    };
    let lo_routes = match count_for(NET_Q_ROUTES, lo.ifindex) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("ip_e2e_test: scoped ROUTES query failed: {}", err.errno());
            return false;
        }
    };
    if lo_routes >= all_routes {
        eprintln!(
            "ip_e2e_test: ROUTES scoped to lo returned {lo_routes} of {all_routes} — not filtered"
        );
        return false;
    }

    // Nothing, not everything — the direction a filter fails in when it is
    // written as "match, or fall through".
    let ghost = 0xDEAD_BEEFu32;
    for (what, name) in [(NET_Q_IFACES, "IFACES"), (NET_Q_ROUTES, "ROUTES")] {
        match count_for(what, ghost) {
            Ok(0) => {}
            Ok(n) => {
                eprintln!("ip_e2e_test: {name} scoped to a missing interface returned {n}, want 0");
                return false;
            }
            Err(err) => {
                eprintln!(
                    "ip_e2e_test: {name} scoped to a missing interface failed: {}",
                    err.errno()
                );
                return false;
            }
        }
    }
    true
}

/// The socket query resolves a *live* TCP connection's real state.
///
/// Pins the two-phase collector's second half: phase one records a placeholder
/// state under the socket-table lock, phase two resolves the real one with that
/// lock released. A phase two resolving nothing looks exactly like "no TCP
/// connections", so the assertion is that the row for *our* connection says
/// ESTABLISHED — a value only phase two can produce.
///
/// The connection goes to QEMU's in-network echo peer so the row exercises a
/// real off-box path over `eth0` rather than loopback. Dialling the peer rather
/// than a public resolver keeps the case answerable on a host with no egress,
/// and the peer is forked per connection, so nothing here contends with another
/// test.
fn socket_query_resolves_a_live_tcp_state() -> bool {
    use std::io::Write as _;
    use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
    use std::time::Duration;

    use slopos_userland::net::{ECHO_PEER_ADDR, ECHO_PEER_PORT};

    let peer = ECHO_PEER_ADDR;
    let addr = SocketAddrV4::new(
        Ipv4Addr::new(peer[0], peer[1], peer[2], peer[3]),
        ECHO_PEER_PORT,
    );
    // Announced before the connect blocks: output is what resets the harness's
    // silence watchdog, and a connect that never completes has nothing else to
    // say for as long as it takes to give up.
    eprintln!("ip_e2e_test: connecting to {addr}");
    let Ok(mut stream) = TcpStream::connect(addr) else {
        eprintln!(
            "ip_e2e_test: {addr} did not answer; cannot open a connection to observe. \
             The peer is a QEMU guestfwd wired up by scripts/qemu_run.sh."
        );
        // Not a pass: without a connection this case cannot answer, and saying
        // "ok" would claim a proof it does not have.
        return false;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    // A byte on the wire, so the connection is established rather than still
    // completing its handshake.
    let _ = stream.write_all(&[0x00, 0x00]);

    let q = match fetch_sockets() {
        Ok(rows) => rows,
        Err(err) => {
            eprintln!("ip_e2e_test: NET_Q_SOCKETS failed: {}", err.errno());
            return false;
        }
    };

    // Matched on the exact four-tuple, not "any ESTABLISHED row": every other
    // process's rows are in this answer too. Ports are host byte order in this
    // struct, unlike `SockAddrIn`, so a byte-order slip surfaces here as a port
    // with its bytes swapped rather than as a missing row.
    let ours = q
        .iter()
        .find(|row| row.remote_addr == peer && row.remote_port == ECHO_PEER_PORT);
    let Some(row) = ours else {
        eprintln!(
            "ip_e2e_test: {} row(s), none to {} — our own connection is missing \
             from a query that returns every socket",
            q.len(),
            addr
        );
        for row in &q {
            eprintln!(
                "ip_e2e_test:   state={} type={} local_port={} remote_port={}",
                row.state, row.sock_type, row.local_port, row.remote_port
            );
        }
        return false;
    };

    eprintln!(
        "ip_e2e_test: live socket state={} local={}.{}.{}.{}:{} peer={}.{}.{}.{}:{} rxq={} txq={}",
        row.state,
        row.local_addr[0],
        row.local_addr[1],
        row.local_addr[2],
        row.local_addr[3],
        row.local_port,
        row.remote_addr[0],
        row.remote_addr[1],
        row.remote_addr[2],
        row.remote_addr[3],
        row.remote_port,
        row.rx_queue,
        row.tx_queue
    );

    if row.state != NET_SOCK_ESTABLISHED {
        eprintln!(
            "ip_e2e_test: our own connection reads state {} rather than ESTABLISHED ({}) \
             — phase two resolved no TCP state",
            row.state, NET_SOCK_ESTABLISHED
        );
        return false;
    }

    // Matched on `ESTAB` rather than the state number: a renderer that mapped
    // the wrong constant would still print *a* state, so the assertion has to
    // be on the text a person reads.
    let mut rendered = false;
    for words in [
        ["ss", "-t"].as_slice(),
        ["ss", "-a"].as_slice(),
        ["ss", "-t", "-p"].as_slice(),
    ] {
        let Some(text) = capture_prog(b"/bin/ss", words) else {
            eprintln!("ip_e2e_test: `{}` produced no output", words.join(" "));
            return false;
        };
        dump(&words.join(" "), &text);
        let needle = std::format!(":{}", row.remote_port);
        if text
            .lines()
            .any(|line| line.contains("ESTAB") && line.contains(&needle))
        {
            rendered = true;
        }
    }
    if !rendered {
        eprintln!(
            "ip_e2e_test: no `ss` view printed an ESTAB row for the live connection — \
             the state reaches the query but not the renderer"
        );
        return false;
    }
    true
}

/// The redaction contract: holding `SYSTEM` and not `NET_ADMIN`, this binary's
/// own socket must name its own pid and no other row may name any pid at all.
/// Disclosing indiscriminately fails the second; redacting indiscriminately, or
/// filtering rows by owner, fails the first.
///
/// Deliberately a bound UDP socket and no traffic, so it reaches the same
/// conclusion on a host with no route off the machine.
fn socket_query_attributes_a_socket_to_its_owner() -> bool {
    use slopos_userland::syscall::net::{bind_any, socket};

    // Distinctive enough to identify this test's row, and outside the ephemeral
    // range the stack allocates from.
    const PORT: u16 = 47251;

    let Ok(fd) = socket(AF_INET as u16, SOCK_DGRAM as u16, 0) else {
        eprintln!("ip_e2e_test: could not create a UDP socket");
        return false;
    };
    if bind_any(fd.raw(), PORT).is_err() {
        eprintln!("ip_e2e_test: could not bind UDP port {PORT}");
        return false;
    }

    let rows = match fetch_sockets() {
        Ok(rows) => rows,
        Err(err) => {
            eprintln!("ip_e2e_test: NET_Q_SOCKETS failed: {}", err.errno());
            return false;
        }
    };

    let mine = process::getpid();
    let named = rows.iter().filter(|r| r.owner_pid != INVALID_PROCESS_ID);
    eprintln!(
        "ip_e2e_test: {} socket row(s) as pid {}, {} naming an owner",
        rows.len(),
        mine,
        named.count()
    );
    for row in &rows {
        eprintln!(
            "ip_e2e_test:   idx={} type={} state={} local_port={} remote_port={} owner={}",
            row.sock_idx, row.sock_type, row.state, row.local_port, row.remote_port, row.owner_pid
        );
    }

    let ours = rows
        .iter()
        .find(|r| r.local_port == PORT && r.sock_type == SOCK_DGRAM as u8);
    let Some(ours) = ours else {
        eprintln!(
            "ip_e2e_test: no row for the UDP socket this process just bound to {PORT} — \
             a query that returns every socket is missing one that exists"
        );
        return false;
    };
    if ours.owner_pid != mine {
        eprintln!(
            "ip_e2e_test: our own socket reports owner {} rather than our pid {} — \
             the owner is withheld from the process that owns it",
            ours.owner_pid, mine
        );
        return false;
    }

    for row in &rows {
        if row.owner_pid != mine && row.owner_pid != INVALID_PROCESS_ID {
            eprintln!(
                "ip_e2e_test: row {}:{} discloses owner {} to an unprivileged caller \
                 whose pid is {}",
                row.local_port, row.remote_port, row.owner_pid, mine
            );
            return false;
        }
    }
    true
}

/// The same rule from a second process, which is the only way to check it.
///
/// *The row must be there*: a diagnostic tool is never the process that opened
/// the socket it was asked about, so a query keyed on the caller's identity
/// would print an empty table to every tool that ran it — invisible from inside
/// a single process, which is why this case spends a spawn.
///
/// *The owner must not be*: `-p` on a socket in another address space prints
/// nothing. There is deliberately no positive `-p` case, because `ss` opens no
/// sockets of its own; that direction is
/// [`socket_query_attributes_a_socket_to_its_owner`].
fn ss_lists_another_processes_socket_without_naming_it() -> bool {
    use slopos_userland::syscall::net::{bind_any, socket};

    // A different port from the query-level case, so a stale row from that one
    // cannot satisfy this one.
    const PORT: u16 = 47252;

    let Ok(fd) = socket(AF_INET as u16, SOCK_DGRAM as u16, 0) else {
        eprintln!("ip_e2e_test: could not create a UDP socket");
        return false;
    };
    if bind_any(fd.raw(), PORT).is_err() {
        eprintln!("ip_e2e_test: could not bind UDP port {PORT}");
        return false;
    }

    // `-a` because a bound datagram socket is not connected, and `ss`'s default
    // view is "what am I talking to".
    let Some(text) = capture_prog(b"/bin/ss", &["ss", "-a", "-p"]) else {
        eprintln!("ip_e2e_test: /bin/ss produced no output");
        return false;
    };
    dump("ss -a -p", &text);

    let needle = std::format!(":{PORT}");
    let row = text
        .lines()
        .find(|line| line.contains(&needle) && !line.contains("Local Address"));
    let Some(row) = row else {
        eprintln!(
            "ip_e2e_test: `ss -a -p` did not list the socket this process bound to {PORT} — \
             a second process cannot see a socket it does not own"
        );
        return false;
    };

    if !row.contains("udp") {
        eprintln!("ip_e2e_test: row for {PORT} is not a udp row: {row}");
        return false;
    }
    if row.contains("users:") {
        eprintln!(
            "ip_e2e_test: `ss -p` named the owner of a socket in another address space: {row}"
        );
        return false;
    }
    true
}

fn fetch_sockets() -> Result<Vec<UserSockInfo>, SyscallError> {
    const SOCK_SIZE: usize = core::mem::size_of::<UserSockInfo>();
    let mut probe = [0u8; HDR];
    net_query(NET_Q_SOCKETS, NET_IFINDEX_NONE, &mut probe)?;
    let total = decode_hdr(&probe).total_count as usize;
    if total == 0 {
        return Ok(Vec::new());
    }
    let mut buf = std::vec![0u8; HDR + total * SOCK_SIZE];
    net_query(NET_Q_SOCKETS, NET_IFINDEX_NONE, &mut buf)?;
    let hdr = decode_hdr(&buf);
    let stride = (hdr.record_size as usize).max(1);
    let mut out = Vec::new();
    for i in 0..hdr.record_count as usize {
        let start = HDR + i * stride;
        out.push(decode_sock(&buf[start..start + stride]));
    }
    Ok(out)
}

/// Decode one `UserSockInfo` by the offsets `abi/src/net.rs` asserts.
fn decode_sock(bytes: &[u8]) -> UserSockInfo {
    let mut out = UserSockInfo::default();
    out.local_addr = [bytes[0], bytes[1], bytes[2], bytes[3]];
    out.remote_addr = [bytes[4], bytes[5], bytes[6], bytes[7]];
    out.local_port = u16::from_le_bytes([bytes[8], bytes[9]]);
    out.remote_port = u16::from_le_bytes([bytes[10], bytes[11]]);
    out.family = bytes[12];
    out.sock_type = bytes[13];
    out.protocol = bytes[14];
    out.state = bytes[15];
    out.owner_pid = u32_at(bytes, 16);
    out.rx_queue = u32_at(bytes, 20);
    out.tx_queue = u32_at(bytes, 24);
    out.sock_idx = u32_at(bytes, 28);
    out
}

/// The monitor fd wakes a reader parked in `poll`.
///
/// The one property nothing else covers: every other netmon test drives the
/// ring directly, exercising the data structure and not the descriptor.
///
/// It proves the event reaches a `poll`-armed descriptor and that a whole
/// `NetEvent` is then readable. It does not prove a hard interleaving — nothing
/// here can hold the child until the parent is inside the syscall — so the
/// assertion is the weaker one, and the quiet pre-check is what keeps it from
/// passing on a stale event.
///
/// Failures quote `lo.admin_up` and the drained count because three distinct
/// causes produce the same timeout.
fn monitor_fd_wakes_a_blocked_poll() -> bool {
    let fd = match net_monitor(NET_MON_DEFAULT, 0) {
        Ok(fd) => fd,
        Err(err) => {
            eprintln!("ip_e2e_test: net_monitor failed: {}", err.errno());
            return false;
        }
    };

    // Drain anything already queued, so what arrives below is caused by the
    // change below and not by whatever the stack did during boot.
    let mut scratch = [0u8; NET_EVENT_LEN * 8];
    let mut drained = 0usize;
    loop {
        let mut probe = [UserPollFd {
            fd: fd.raw(),
            events: POLLIN,
            revents: 0,
        }];
        match fs::poll(&mut probe, 0) {
            Ok(0) => break,
            Ok(_) => match fs::read_slice(fd.raw(), &mut scratch) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    drained += n / NET_EVENT_LEN;
                    continue;
                }
            },
            Err(err) => {
                eprintln!("ip_e2e_test: drain poll failed: {}", err.errno());
                return false;
            }
        }
    }

    // The stack posts on its own schedule (DHCP timers, carrier polls), so an
    // event landing between the drain above and this check means the machine is
    // busy. Only a monitor that never settles is a finding.
    let mut settled = false;
    for _ in 0..8 {
        let mut quiet = [UserPollFd {
            fd: fd.raw(),
            events: POLLIN,
            revents: 0,
        }];
        match fs::poll(&mut quiet, 0) {
            Ok(0) => {
                settled = true;
                break;
            }
            Ok(_) => match fs::read_slice(fd.raw(), &mut scratch) {
                Ok(0) | Err(_) => break,
                Ok(n) => drained += n / NET_EVENT_LEN,
            },
            Err(err) => {
                eprintln!("ip_e2e_test: quiet poll failed: {}", err.errno());
                return false;
            }
        }
    }
    if !settled {
        eprintln!(
            "ip_e2e_test: the monitor never went quiet — {drained} event(s) drained and \
             still ready. Something is posting continuously."
        );
        return false;
    }

    // A request that changes nothing is not an event: with `lo` already down,
    // `set lo down` correctly posts nothing and the wait correctly expires,
    // which is indistinguishable from a producer or a wake that never fired.
    let (lo_ifindex, admin_up_before) = match read_ifaces() {
        Ok((_, rows)) => match find(&rows, "lo") {
            Some(lo) => (lo.ifindex, lo.admin_up),
            None => {
                eprintln!("ip_e2e_test: no `lo` in the interface table");
                return false;
            }
        },
        Err(err) => {
            eprintln!(
                "ip_e2e_test: reading lo before the change failed: {}",
                err.errno()
            );
            return false;
        }
    };
    if admin_up_before == 0 {
        // Not repaired on purpose: bringing it back would hide whatever left
        // the interface down while making this case pass.
        eprintln!(
            "ip_e2e_test: lo is already down before `ip link set lo down` — the change \
             would post nothing. The fault is upstream of this case: an earlier test or \
             run left the interface down."
        );
        return false;
    }

    let down = spawn_prog(b"/bin/ip", &["ip", "link", "set", "lo", "down"], 1);
    if down <= 0 {
        eprintln!("ip_e2e_test: spawn of `ip link set lo down` returned {down}");
        return false;
    }

    // Waits for an event naming `lo`, not merely for the descriptor to become
    // readable: an unrelated interface's event arriving first would otherwise
    // read as the answer. The deadline is on the whole wait, not on each poll,
    // so a busy stack cannot extend it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    let mut unrelated = 0usize;
    let mut ok = false;
    let mut expired = false;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            expired = true;
            break;
        }
        let mut armed = [UserPollFd {
            fd: fd.raw(),
            events: POLLIN,
            revents: 0,
        }];
        match fs::poll(
            &mut armed,
            remaining.as_millis().min(i64::MAX as u128) as i64,
        ) {
            Ok(0) => {
                expired = true;
                break;
            }
            Ok(_) if armed[0].revents & POLLIN == 0 => {
                eprintln!(
                    "ip_e2e_test: poll returned revents={:#x} without POLLIN",
                    armed[0].revents
                );
                break;
            }
            Ok(_) => match read_one_event(fd.raw()) {
                Some(event) if event.ifindex == lo_ifindex => {
                    ok = true;
                    break;
                }
                Some(_) => unrelated += 1,
                None => break,
            },
            Err(err) => {
                eprintln!("ip_e2e_test: armed poll failed: {}", err.errno());
                break;
            }
        }
    }
    let down_status = process::waitpid(down as u32);
    if expired {
        eprintln!(
            "ip_e2e_test: no event for lo (ifindex {lo_ifindex}) within 2000 ms. \
             lo.admin_up was {admin_up_before} before the change, `ip link set lo down` \
             exited {down_status}, {drained} event(s) drained beforehand and {unrelated} \
             for other interfaces meanwhile. admin_up=1 with a zero exit means the \
             producer or the wake did not fire."
        );
    }

    // Restore loopback whatever happened above; a test that leaves the stack
    // broken makes every later one lie.
    let up = run_ip(&["ip", "link", "set", "lo", "up"]);
    if up != 0 {
        eprintln!("ip_e2e_test: restoring lo returned {up}");
        return false;
    }
    match read_ifaces() {
        Ok((_, rows)) => match find(&rows, "lo") {
            Some(lo) if lo.admin_up != 0 => {}
            _ => {
                eprintln!("ip_e2e_test: lo did not come back up");
                return false;
            }
        },
        Err(_) => return false,
    }

    ok
}

/// Read one whole `NetEvent` off a readable monitor fd and sanity-check it.
///
/// `None` means the descriptor misbehaved, never "not the event wanted" — the
/// caller decides which events it is waiting for.
fn read_one_event(fd: i32) -> Option<NetEvent> {
    let mut buf = [0u8; NET_EVENT_LEN];
    let n = match fs::read_slice(fd, &mut buf) {
        Ok(n) => n,
        Err(err) => {
            eprintln!(
                "ip_e2e_test: read from a readable monitor failed: {}",
                err.errno()
            );
            return None;
        }
    };
    if n != NET_EVENT_LEN {
        eprintln!("ip_e2e_test: read {n} bytes, want a whole {NET_EVENT_LEN}-byte record");
        return None;
    }

    let event = NetEvent::from_bytes(&buf);
    eprintln!(
        "ip_e2e_test: event kind={} ifindex={} seq={}",
        event.kind, event.ifindex, event.seq
    );
    if event.kind == 0 || event.kind > 13 {
        eprintln!("ip_e2e_test: event kind {} is outside NET_EV_*", event.kind);
        return None;
    }
    if event.ifindex == NET_IFINDEX_NONE {
        eprintln!("ip_e2e_test: event names no interface");
        return None;
    }
    if event.seq == 0 {
        eprintln!("ip_e2e_test: event carries sequence 0");
        return None;
    }
    Some(event)
}

/// What `/bin/ip` actually renders, captured through a pipe and printed.
///
/// The structural assertion is deliberately narrow — the `lo` line exists and
/// says `UNKNOWN` — because pinning the whole layout would make every cosmetic
/// change a test failure. The captured text is emitted in full instead.
fn ip_renders_the_stack() -> bool {
    let commands: [&[&str]; 7] = [
        &["ip", "link"],
        &["ip", "-br", "addr"],
        &["ip", "route"],
        &["ip", "status"],
        // The read half of the master switch. The write half is deliberately
        // absent: turning it off downs every device, taking unrelated socket
        // and NAPI tests with it.
        &["ip", "net"],
        &["ip", "link", "show", "dev", "lo"],
        &["ip", "-s", "link", "show", "dev", "lo"],
    ];

    let mut link_output = String::new();
    for words in commands {
        let label = words.join(" ");
        let Some(text) = capture_ip(words) else {
            eprintln!("ip_e2e_test: `{label}` produced no output");
            return false;
        };
        dump(&label, &text);
        if text.trim().is_empty() {
            eprintln!("ip_e2e_test: `{label}` printed nothing");
            return false;
        }
        if words == ["ip", "link"] {
            link_output = text.clone();
        }
        // The numbered header lines are one per interface, so counting them is
        // enough to tell a scoped render from the whole table.
        if words.contains(&"dev") {
            let headers = text
                .lines()
                .filter(|line| line.contains(": lo:") || line.contains(": eth0:"))
                .count();
            if headers != 1 {
                eprintln!("ip_e2e_test: `{label}` rendered {headers} interfaces, want 1");
                return false;
            }
            if !text.contains("lo:") {
                eprintln!("ip_e2e_test: `{label}` did not render lo");
                return false;
            }
        }
    }

    // The renderer's half of `loopback_operstate_is_unknown`.
    let lo_line = link_output
        .lines()
        .find(|line| line.contains("lo:") && line.contains("LOOPBACK"));
    let Some(lo_line) = lo_line else {
        eprintln!("ip_e2e_test: `ip link` has no loopback line");
        return false;
    };
    if !lo_line.contains("state UNKNOWN") {
        eprintln!("ip_e2e_test: loopback line does not say UNKNOWN: {lo_line}");
        return false;
    }
    true
}

/// `neigh`, `dns` and `route` must answer for real rather than the "not
/// supported" the CLI renders an `ENOSYS` as. The route case is a write
/// round-trip on `203.0.113.0/24` (TEST-NET-3, which nothing routes to), so it
/// proves `net_route_ctl` both directions without disturbing the live stack.
fn late_verbs_are_served() -> bool {
    let Some(neigh) = capture_ip(&["ip", "neigh"]) else {
        eprintln!("ip_e2e_test: `ip neigh` produced no output");
        return false;
    };
    if neigh.contains("not supported") || neigh.contains("ENOSYS") {
        eprintln!("ip_e2e_test: `ip neigh` still reports unsupported: {neigh}");
        return false;
    }

    // SLIRP supplies a nameserver in the lease, so an empty resolver here means
    // the lease was taken and its DNS option dropped on the floor.
    let Some(dns) = capture_ip(&["ip", "dns"]) else {
        eprintln!("ip_e2e_test: `ip dns` produced no output");
        return false;
    };
    if !dns.contains("nameserver") {
        eprintln!("ip_e2e_test: `ip dns` named no nameserver: [{dns}]");
        return false;
    }

    const TEST_NET_3: &str = "203.0.113.0/24";
    let before = capture_ip(&["ip", "route"]).unwrap_or_default();
    if before.contains(TEST_NET_3) {
        eprintln!("ip_e2e_test: {TEST_NET_3} was already routed; test cannot own it");
        return false;
    }

    if run_ip(&[
        "ip", "route", "add", TEST_NET_3, "via", "10.0.2.2", "dev", "eth0",
    ]) != 0
    {
        eprintln!("ip_e2e_test: `ip route add` failed");
        return false;
    }
    let during = capture_ip(&["ip", "route"]).unwrap_or_default();
    if !during.contains(TEST_NET_3) {
        eprintln!("ip_e2e_test: added route is absent from `ip route`: {during}");
        return false;
    }

    if run_ip(&["ip", "route", "del", TEST_NET_3]) != 0 {
        eprintln!("ip_e2e_test: `ip route del` failed — the test route is still installed");
        return false;
    }
    let after = capture_ip(&["ip", "route"]).unwrap_or_default();
    if after.contains(TEST_NET_3) {
        eprintln!("ip_e2e_test: deleted route survives: {after}");
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("late_verbs_are_served", late_verbs_are_served),
    (
        "iface_table_has_lo_and_ethernet",
        iface_table_has_lo_and_ethernet,
    ),
    (
        "loopback_operstate_is_unknown",
        loopback_operstate_is_unknown,
    ),
    (
        "truncation_is_reported_in_the_header",
        truncation_is_reported_in_the_header,
    ),
    (
        "queries_and_monitor_are_unprivileged",
        queries_and_monitor_are_unprivileged,
    ),
    (
        "direct_mutation_is_eperm_despite_system",
        direct_mutation_is_eperm_despite_system,
    ),
    (
        "queries_honour_their_ifindex_filter",
        queries_honour_their_ifindex_filter,
    ),
    (
        "socket_query_attributes_a_socket_to_its_owner",
        socket_query_attributes_a_socket_to_its_owner,
    ),
    (
        "socket_query_resolves_a_live_tcp_state",
        socket_query_resolves_a_live_tcp_state,
    ),
    (
        "ss_lists_another_processes_socket_without_naming_it",
        ss_lists_another_processes_socket_without_naming_it,
    ),
    (
        "monitor_fd_wakes_a_blocked_poll",
        monitor_fd_wakes_a_blocked_poll,
    ),
    ("ip_renders_the_stack", ip_renders_the_stack),
];

fn main() {
    slopos_slibc::test_harness::run_with_progress("ip_e2e", CASES);
}
