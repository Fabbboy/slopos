//! `ip` — the network configuration command.
//!
//! The grammar lives in [`slopos_net_core::ip_plan`] and is decided before any
//! syscall runs; this crate's job is to execute a [`Plan`] and render what came
//! back. Keeping the two apart is what makes the grammar host-testable: a
//! `Plan` holds no descriptors and performs no I/O, so `cargo test -p
//! slopos-net-core` covers every parsing decision without a kernel.
//!
//! Every state→string mapping comes from [`slopos_net_core::render`], which the
//! compositor's status indicator also reads. `ip link` and the bar name the
//! same states, so if either spelled them itself a person reading a terminal
//! and a panel at once would get two answers to one question.
//!
//! # What is not here
//!
//! Several objects the grammar accepts are not served by this kernel:
//! `NET_Q_NEIGH`, `NET_Q_SOCKETS`, `NET_Q_RESOLVER` and `NET_Q_DHCP` answer
//! `ENOSYS`, as do the DHCP operations and the three mutation syscalls that
//! have no dispatch entry yet. Each of those prints
//! `ip: OBJECT: not supported by this kernel yet` and exits 1 — never an empty
//! table. "You have no neighbours" and "this kernel cannot tell you" are
//! different answers, and a person must be able to tell which one they got.
//!
//! Nothing here pre-checks whether the caller holds `NET_ADMIN`. There is no
//! syscall to read a task's own flags, so any such check would be a guess that
//! can disagree with the kernel — which is how a tool comes to print an error
//! for something it went on to do anyway. The syscall is issued and its errno
//! is rendered.

mod addr;
mod dhcp;
mod dns;
mod help;
mod link;
mod monitor;
mod neigh;
mod net;
mod route;
mod status;

use std::string::{String, ToString};
use std::vec::Vec;

use slopos_net_core::ip_plan::{IpError, Options, Plan, parse};

use crate::net_query as query;
use crate::syscall::SyscallError;

/// What a command that did not run has to say, as the two halves of the
/// `ip: CONTEXT: MESSAGE` line plus the status it earns.
pub struct Failure {
    context: String,
    message: String,
    status: i32,
}

/// Exit status for a command that ran and failed.
const EXIT_RUNTIME: i32 = 1;
/// Exit status for a command line that is not a command.
const EXIT_USAGE: i32 = 2;

/// What the kernel says when an object is defined in the ABI and not yet
/// served. Spelled once so every object reports it identically.
const NOT_SUPPORTED: &str = "not supported by this kernel yet";

impl Failure {
    pub fn runtime(context: impl Into<String>, message: impl Into<String>) -> Failure {
        Failure {
            context: context.into(),
            message: message.into(),
            status: EXIT_RUNTIME,
        }
    }

    pub fn usage(context: impl Into<String>, message: impl Into<String>) -> Failure {
        Failure {
            context: context.into(),
            message: message.into(),
            status: EXIT_USAGE,
        }
    }

    /// Render a syscall's errno for a person.
    ///
    /// `EPERM` gets the extra clause because a system with no uids has no
    /// `sudo`: a bare "operation not permitted" sends the reader hunting for a
    /// command that does not exist, when the actual answer is that the
    /// privilege comes from the binary's path.
    pub fn from_errno(context: impl Into<String>, err: SyscallError) -> Failure {
        let message = match err {
            SyscallError::EPERM => "operation not permitted (need NET_ADMIN; run /bin/ip)",
            SyscallError::ENOSYS => NOT_SUPPORTED,
            SyscallError::ENODEV => "no such device",
            SyscallError::EBUSY => "another administrative change is in flight",
            SyscallError::EINVAL => "invalid argument",
            SyscallError::ENOSPC => "no room for another entry",
            SyscallError::ENOENT => "no such entry",
            other => return Failure::runtime(context, other.as_str()),
        };
        Failure::runtime(context, message)
    }

    /// The kernel does not serve this object yet.
    pub fn unsupported(object: &str) -> Failure {
        Failure::runtime(object, NOT_SUPPORTED)
    }
}

pub type Outcome = Result<(), Failure>;

/// How long `ip monitor` runs when neither bound is given.
///
/// Bounded by default on purpose: a monitor that blocks forever is
/// indistinguishable from a hang in a serial transcript, and this is the only
/// user-runnable demonstration of a pollable fd in the tree. `-t 0` asks for no
/// deadline, for someone who wants to watch.
const DEFAULT_MONITOR_MS: i64 = 10_000;

/// The bounds `ip monitor` runs under.
#[derive(Clone, Copy)]
pub struct MonitorBounds {
    /// Stop after this many events, or `None` for no count limit.
    pub count: Option<u32>,
    /// Stop after this many milliseconds, or `None` for no deadline.
    pub deadline_ms: Option<i64>,
}

impl Default for MonitorBounds {
    fn default() -> Self {
        Self {
            count: None,
            deadline_ms: Some(DEFAULT_MONITOR_MS),
        }
    }
}

pub fn ip_main(args: Vec<String>) -> ! {
    let program = args.first().map(String::as_str).unwrap_or("ip");
    let status = if basename(program) == "ifconfig" {
        run_ifconfig(&args[1.min(args.len())..])
    } else {
        run_ip(&args[1.min(args.len())..])
    };
    std::process::exit(status)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `ifconfig` is this binary under another name, sharing this renderer rather
/// than being a second implementation.
///
/// The shell passes the typed name as `argv[0]` and resolves the path through
/// the program registry, so one extra registry entry pointing at `/bin/ip` is
/// the whole of the alias. It renders `ip addr show` — the addresses, hardware
/// address and MTU `ifconfig` is expected to print — over the interfaces that
/// exist rather than a hardcoded name.
fn run_ifconfig(args: &[String]) -> i32 {
    let plan = match args {
        [] => Plan::AddrShow { dev: None },
        [dev] if !dev.starts_with('-') => Plan::AddrShow {
            dev: Some(dev.as_bytes()),
        },
        _ => {
            report(&Failure::usage(
                "ifconfig",
                "usage: ifconfig [DEVICE]  (this is /bin/ip; type `ip help` for the full grammar)",
            ));
            return EXIT_USAGE;
        }
    };
    finish(execute(&plan, Options::default(), MonitorBounds::default()))
}

fn run_ip(args: &[String]) -> i32 {
    let (bounds, bounds_given, rest) = match take_monitor_bounds(args) {
        Ok(taken) => taken,
        Err(failure) => {
            report(&failure);
            return failure.status;
        }
    };

    let tokens: Vec<&[u8]> = rest.iter().map(|arg| arg.as_bytes()).collect();
    let invocation = match parse(&tokens) {
        Ok(invocation) => invocation,
        Err(err) => {
            let failure = describe(err);
            report(&failure);
            if matches!(err, IpError::Usage) {
                help::usage();
            }
            return failure.status;
        }
    };

    // The two monitor bounds are handled here rather than in the grammar, so
    // accepting them for an object that cannot use them would be silent.
    if bounds_given && !matches!(invocation.plan, Plan::Monitor { .. }) {
        report(&Failure::usage(
            "-c/-t",
            "only `ip monitor` takes a count or a deadline",
        ));
        return EXIT_USAGE;
    }

    finish(execute(&invocation.plan, invocation.opts, bounds))
}

fn finish(outcome: Outcome) -> i32 {
    match outcome {
        Ok(()) => 0,
        Err(failure) => {
            report(&failure);
            failure.status
        }
    }
}

fn report(failure: &Failure) {
    eprintln!("ip: {}: {}", failure.context, failure.message);
}

/// Consume `-c COUNT` and `-t MILLISECONDS` from the front of the line.
///
/// They are stripped before [`parse`] rather than added to the grammar: the
/// grammar is a closed table `net-core` owns and host-tests, and a bound on how
/// long a process runs is a property of this binary, not of the language. The
/// leading-option run is the only place they are looked for, which is where the
/// grammar puts every other option.
///
/// Returns the bounds, whether either was given, and the remaining arguments.
fn take_monitor_bounds(args: &[String]) -> Result<(MonitorBounds, bool, &[String]), Failure> {
    let mut bounds = MonitorBounds::default();
    let mut given = false;
    let mut idx = 0usize;

    while idx < args.len() {
        let flag = args[idx].as_str();
        let key = match flag {
            "-c" | "-t" => flag,
            _ => break,
        };
        let Some(value) = args.get(idx + 1) else {
            return Err(Failure::usage(key, "expects a number"));
        };
        match key {
            "-c" => {
                let count: u32 = value
                    .parse()
                    .map_err(|_| Failure::usage(key, "expects a count"))?;
                bounds.count = Some(count);
                // A count with no explicit deadline still needs one: the events
                // may simply never arrive.
            }
            _ => {
                let ms: i64 = value
                    .parse()
                    .map_err(|_| Failure::usage(key, "expects milliseconds"))?;
                bounds.deadline_ms = if ms <= 0 { None } else { Some(ms) };
            }
        }
        given = true;
        idx += 2;
    }

    Ok((bounds, given, &args[idx..]))
}

fn execute(plan: &Plan<'_>, opts: Options, bounds: MonitorBounds) -> Outcome {
    match *plan {
        Plan::LinkShow { dev } => link::show(dev, opts),
        Plan::LinkSet { dev, up } => link::set(dev, up),

        Plan::AddrShow { dev } => addr::show(dev, opts),
        Plan::AddrAdd { cidr, dev } => addr::add(cidr, dev, true),
        Plan::AddrDel { cidr, dev } => addr::add(cidr, dev, false),
        Plan::AddrFlush { dev } => addr::flush(dev),

        Plan::RouteShow { dev } => route::show(dev),
        Plan::RouteAdd { dest, via, dev } => route::change(dest, Some(via), Some(dev), true),
        Plan::RouteDel { dest, dev } => route::change(dest, None, dev, false),

        Plan::NeighShow { dev } => neigh::show(dev),
        Plan::NeighDel { addr, dev } => neigh::del(addr, dev),
        Plan::NeighFlush { dev } => neigh::flush(dev),

        Plan::DhcpStart { dev } => dhcp::op(dev, dhcp::Op::Start),
        Plan::DhcpStop { dev } => dhcp::op(dev, dhcp::Op::Stop),
        Plan::DhcpRenew { dev } => dhcp::op(dev, dhcp::Op::Renew),
        Plan::DhcpRelease { dev } => dhcp::op(dev, dhcp::Op::Release),
        Plan::DhcpStatus { dev } => dhcp::status(dev),

        Plan::DnsShow => dns::show(),
        Plan::DnsSet { servers, count } => dns::set(&servers[..count as usize]),

        Plan::NetShow => net::show(),
        Plan::NetSet { enabled } => net::set(enabled),

        Plan::Monitor { filter } => monitor::run(filter, bounds),
        Plan::Status => status::show(),
        Plan::Help { object } => {
            help::print(object);
            Ok(())
        }
    }
}

/// Turn a grammar error into the line a person reads.
fn describe(err: IpError<'_>) -> Failure {
    match err {
        IpError::Usage => Failure::usage("usage", "an object is required"),
        IpError::UnknownObject { token } => {
            Failure::usage(text(token), "unknown object; try `ip help`")
        }
        IpError::AmbiguousObject { token, table } => {
            Failure::usage(text(token), candidates("ambiguous object", token, table))
        }
        IpError::UnknownCommand { token, object } => Failure::usage(
            text(token),
            std::format!("unknown command for `ip {}`", object.name()),
        ),
        IpError::AmbiguousCommand { token, table } => {
            Failure::usage(text(token), candidates("ambiguous command", token, table))
        }
        IpError::MissingKeyword(keyword) => {
            Failure::usage("usage", std::format!("expected `{keyword}`"))
        }
        IpError::MissingOperand => Failure::usage("usage", "missing operand"),
        IpError::BadCidr { token } => Failure::usage(text(token), "not an address/prefix"),
        IpError::BadAddr { token } => Failure::usage(text(token), "not an IPv4 address"),
        IpError::BadDevice { token } => Failure::usage(text(token), "not a device name"),
        IpError::OptionAfterObject { token } => {
            Failure::usage(text(token), "options must precede the object")
        }
        IpError::UnknownOption { token } => Failure::usage(text(token), "unknown option"),
        IpError::TrailingOperand => Failure::usage("usage", "too many operands"),
    }
}

/// List what an abbreviation could have meant. An ambiguity report that does
/// not name the candidates leaves the reader to guess which word to lengthen.
fn candidates(lead: &str, token: &[u8], table: &'static [&'static str]) -> String {
    let mut out = String::from(lead);
    out.push_str("; could be");
    let mut first = true;
    for name in slopos_net_core::argv::matches(token, table) {
        out.push_str(if first { " " } else { ", " });
        out.push_str(name);
        first = false;
    }
    out
}

/// An argument as text for an error message. Arguments arrive as `String`s, so
/// this only has to survive the byte slices the grammar hands back.
fn text(token: &[u8]) -> String {
    match core::str::from_utf8(token) {
        Ok(s) => s.to_string(),
        Err(_) => String::from("<non-utf8>"),
    }
}

/// Resolve a device name to its index, or say which name was wrong.
pub fn device_index(ifaces: &query::Ifaces, dev: &[u8]) -> Result<u32, Failure> {
    match ifaces.find(dev) {
        Some(iface) => Ok(iface.ifindex),
        None => Err(Failure::runtime(text(dev), "no such device")),
    }
}

/// Load the interface table, which every renderer needs to turn an index into a
/// name.
pub fn load_ifaces(context: &str) -> Result<query::Ifaces, Failure> {
    query::Ifaces::fetch().map_err(|err| Failure::from_errno(context, err))
}

/// Note that a snapshot grew while it was being read. Not an error: the state
/// is live, and showing what arrived beats refusing to show anything.
pub fn warn_truncated(object: &str, shown: usize, total: u32) {
    eprintln!("ip: {object}: showing {shown} of {total} (the table changed while reading)");
}
