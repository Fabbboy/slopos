//! `ss` — socket statistics, and `netstat` under the same roof: the shell
//! passes the typed name as `argv[0]` and both names share one renderer.
//!
//! **Every row is visible to everyone; the owner is not.** `NET_Q_SOCKETS`
//! reports `owner_pid` only for rows the caller owns or to a caller holding
//! `NET_ADMIN`, so `-p` prints what it was given and never guesses: a redacted
//! owner and an unowned socket arrive as the same value by design.

use core::fmt::Write;
use std::string::String;
use std::vec::Vec;

use slopos_abi::net::{
    NET_IFINDEX_NONE, NET_Q_IFACES, NET_Q_ROUTES, NET_Q_SOCKETS, NET_SOCK_LISTEN, SOCK_DGRAM,
    SOCK_STREAM, UserIface, UserRoute, UserSockInfo,
};
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_net_core::Ipv4;
use slopos_net_core::argv::scan_bundled;
use slopos_net_core::columns::field;
use slopos_net_core::render::{route_origin, sock_state, sock_transport};
use slopos_net_core::ss_filter::{is_connected, ss_row_selected};

use crate::net_query::{self, name_of};

// The four that decide which rows are shown are `net-core`'s, so the flag
// algebra is host-tested rather than only reachable through a boot.
const OPT_TCP: u32 = slopos_net_core::ss_filter::SS_TCP;
const OPT_UDP: u32 = slopos_net_core::ss_filter::SS_UDP;
const OPT_LISTEN: u32 = slopos_net_core::ss_filter::SS_LISTEN;
const OPT_ALL: u32 = slopos_net_core::ss_filter::SS_ALL;
const OPT_NUMERIC: u32 = 1 << 4;
const OPT_PROCESS: u32 = 1 << 5;
const OPT_SUMMARY: u32 = 1 << 6;
/// `netstat -r`: the routing table.
const OPT_ROUTE: u32 = 1 << 7;
/// `netstat -i`: per-interface counters.
const OPT_IFACE: u32 = 1 << 8;

const SS_FLAGS: &[(u8, u32)] = &[
    (b't', OPT_TCP),
    (b'u', OPT_UDP),
    (b'l', OPT_LISTEN),
    (b'a', OPT_ALL),
    (b'n', OPT_NUMERIC),
    (b'p', OPT_PROCESS),
    (b's', OPT_SUMMARY),
];

/// `netstat` adds the two views that are a different question — the routing
/// table and the interface counters — which is why the alias carries its own
/// flag table rather than being a pure rename.
const NETSTAT_FLAGS: &[(u8, u32)] = &[
    (b't', OPT_TCP),
    (b'u', OPT_UDP),
    (b'l', OPT_LISTEN),
    (b'a', OPT_ALL),
    (b'n', OPT_NUMERIC),
    (b'p', OPT_PROCESS),
    (b's', OPT_SUMMARY),
    (b'r', OPT_ROUTE),
    (b'i', OPT_IFACE),
];

// Column widths. `Netid` holds `unknown`, `State` holds `FIN-WAIT-1`, and an
// address column holds `255.255.255.255:65535`.
const COL_NETID: usize = 6;
const COL_STATE: usize = 11;
const COL_QUEUE: usize = 7;
const COL_ADDR: usize = 22;

pub fn ss_main(args: Vec<String>) -> ! {
    let program = args.first().map(String::as_str).unwrap_or("ss");
    let netstat = basename(program) == "netstat";
    let table = if netstat { NETSTAT_FLAGS } else { SS_FLAGS };

    let mut opts = 0u32;
    for arg in args.iter().skip(1) {
        if !arg.starts_with('-') || arg.len() < 2 {
            eprintln!("{}: {}: not an option", basename(program), arg);
            std::process::exit(2);
        }
        match scan_bundled(arg.as_bytes(), table) {
            Ok(bits) => opts |= bits,
            Err(flag) => {
                eprintln!("{}: -{}: unknown option", basename(program), flag as char);
                usage(netstat);
                std::process::exit(2);
            }
        }
    }

    let status = run(opts, netstat);
    std::process::exit(status)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn run(opts: u32, netstat: bool) -> i32 {
    if opts & OPT_ROUTE != 0 {
        return show_routes();
    }
    if opts & OPT_IFACE != 0 {
        return show_ifaces();
    }

    let q = match net_query::fetch::<UserSockInfo>(NET_Q_SOCKETS, NET_IFINDEX_NONE) {
        Ok(q) => q,
        Err(err) => {
            let name = if netstat { "netstat" } else { "ss" };
            if err == crate::syscall::SyscallError::ENOSYS {
                eprintln!("{name}: sockets: not supported by this kernel yet");
            } else {
                eprintln!("{name}: sockets: {}", err.as_str());
            }
            return 1;
        }
    };

    let rows: Vec<&UserSockInfo> = q.records.iter().filter(|row| selected(row, opts)).collect();

    if opts & OPT_SUMMARY != 0 {
        return summary(&q.records);
    }

    print_header(opts);
    for row in rows {
        print_row(row, opts);
    }
    0
}

/// A wrongly dropped row looks exactly like having no sockets, so the decision
/// itself lives in host-tested [`slopos_net_core::ss_filter::ss_row_selected`].
fn selected(row: &UserSockInfo, opts: u32) -> bool {
    ss_row_selected(opts, row.sock_type, row.state)
}

fn print_header(opts: u32) {
    let mut line = String::new();
    let _ = field(&mut line, "Netid", COL_NETID);
    let _ = field(&mut line, "State", COL_STATE);
    let _ = field(&mut line, "Recv-Q", COL_QUEUE);
    let _ = field(&mut line, "Send-Q", COL_QUEUE);
    let _ = field(&mut line, "Local Address:Port", COL_ADDR);
    let _ = field(&mut line, "Peer Address:Port", COL_ADDR);
    if opts & OPT_PROCESS != 0 {
        line.push_str("Process");
    }
    println!("{}", line.trim_end());
}

fn print_row(row: &UserSockInfo, opts: u32) {
    let mut line = String::new();
    let _ = field(
        &mut line,
        sock_transport(row.sock_type, row.protocol),
        COL_NETID,
    );
    let _ = field(&mut line, sock_state(row.state), COL_STATE);

    let mut num = String::new();
    let _ = write!(num, "{}", row.rx_queue);
    let _ = field(&mut line, &num, COL_QUEUE);
    num.clear();
    let _ = write!(num, "{}", row.tx_queue);
    let _ = field(&mut line, &num, COL_QUEUE);

    let mut addr = String::new();
    let _ = write!(addr, "{}", Endpoint(row.local_addr, row.local_port));
    let _ = field(&mut line, &addr, COL_ADDR);
    addr.clear();
    let _ = write!(addr, "{}", Endpoint(row.remote_addr, row.remote_port));
    let _ = field(&mut line, &addr, COL_ADDR);

    // Linux's shape is `users:(("comm",pid=N,fd=N))`; this ABI carries neither
    // the command name nor the descriptor, so the single entry keeps the
    // brackets and drops the rest.
    if opts & OPT_PROCESS != 0 && row.owner_pid != INVALID_PROCESS_ID {
        let _ = write!(line, "users:((pid={}))", row.owner_pid);
    }
    println!("{}", line.trim_end());
}

/// The wildcard address and a zero port print as `*`, so a listening row reads
/// as "any interface" at a glance.
struct Endpoint([u8; 4], u16);

impl core::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let addr = Ipv4(self.0);
        if addr.is_unspecified() {
            f.write_str("*")?;
        } else {
            write!(f, "{addr}")?;
        }
        f.write_str(":")?;
        if self.1 == 0 {
            f.write_str("*")
        } else {
            write!(f, "{}", self.1)
        }
    }
}

/// `-s`: counted over every row the kernel returned, not over the filtered set —
/// a summary that respected `-l` would summarise one flag, not the machine.
fn summary(rows: &[UserSockInfo]) -> i32 {
    let total = rows.len();
    let tcp = rows
        .iter()
        .filter(|r| r.sock_type == SOCK_STREAM as u8)
        .count();
    let udp = rows
        .iter()
        .filter(|r| r.sock_type == SOCK_DGRAM as u8)
        .count();
    let listening = rows.iter().filter(|r| r.state == NET_SOCK_LISTEN).count();
    let connected = rows.iter().filter(|r| is_connected(r.state)).count();

    println!("Total: {total}");
    println!("TCP:   {tcp} ({listening} listening, {connected} connected)");
    println!("UDP:   {udp}");
    0
}

/// `netstat -r`.
fn show_routes() -> i32 {
    let Ok(ifaces) = net_query::Ifaces::fetch() else {
        eprintln!("netstat: interfaces: cannot read the interface table");
        return 1;
    };
    let q = match net_query::fetch::<UserRoute>(NET_Q_ROUTES, NET_IFINDEX_NONE) {
        Ok(q) => q,
        Err(err) => {
            eprintln!("netstat: routes: {}", err.as_str());
            return 1;
        }
    };

    let mut line = String::new();
    let _ = field(&mut line, "Destination", COL_ADDR);
    let _ = field(&mut line, "Gateway", COL_ADDR);
    let _ = field(&mut line, "Iface", 10);
    line.push_str("Proto");
    println!("{}", line.trim_end());

    for route in &q.records {
        line.clear();
        let mut cell = String::new();
        if route.prefix_len == 0 && Ipv4(route.prefix).is_unspecified() {
            cell.push_str("default");
        } else {
            let _ = write!(cell, "{}/{}", Ipv4(route.prefix), route.prefix_len);
        }
        let _ = field(&mut line, &cell, COL_ADDR);

        cell.clear();
        let gateway = Ipv4(route.gateway);
        if gateway.is_unspecified() {
            cell.push('*');
        } else {
            let _ = write!(cell, "{gateway}");
        }
        let _ = field(&mut line, &cell, COL_ADDR);

        let _ = field(
            &mut line,
            &net_query::name_or_index(&ifaces, route.ifindex),
            10,
        );
        line.push_str(route_origin(route.origin));
        println!("{}", line.trim_end());
    }
    0
}

/// `netstat -i`.
fn show_ifaces() -> i32 {
    let q = match net_query::fetch::<UserIface>(NET_Q_IFACES, NET_IFINDEX_NONE) {
        Ok(q) => q,
        Err(err) => {
            eprintln!("netstat: interfaces: {}", err.as_str());
            return 1;
        }
    };

    let mut line = String::new();
    let _ = field(&mut line, "Iface", 10);
    let _ = field(&mut line, "MTU", 7);
    let _ = field(&mut line, "RX-OK", 12);
    let _ = field(&mut line, "RX-ERR", 9);
    let _ = field(&mut line, "TX-OK", 12);
    let _ = field(&mut line, "TX-ERR", 9);
    println!("{}", line.trim_end());

    for iface in &q.records {
        line.clear();
        let _ = field(&mut line, name_of(iface), 10);
        let mut cell = String::new();
        for (value, width) in [
            (u64::from(iface.mtu), 7usize),
            (iface.rx_packets, 12),
            (iface.rx_errors, 9),
            (iface.tx_packets, 12),
            (iface.tx_errors, 9),
        ] {
            cell.clear();
            let _ = write!(cell, "{value}");
            let _ = field(&mut line, &cell, width);
        }
        println!("{}", line.trim_end());
    }
    0
}

fn usage(netstat: bool) {
    if netstat {
        println!("usage: netstat [-tulanps] [-r] [-i]");
        println!("  -r  routing table      -i  per-interface counters");
    } else {
        println!("usage: ss [-tulanps]");
    }
    println!("  -t  TCP    -u  UDP    -l  listening    -a  all");
    println!("  -n  numeric (addresses are never resolved anyway)");
    println!("  -p  show the owning pid    -s  summary");
    println!();
    println!("Every socket is listed. -p names the owner only for sockets you");
    println!("own; a caller holding NET_ADMIN sees every owner. A blank Process");
    println!("column means either nobody owns it or you may not be told who does.");
}
