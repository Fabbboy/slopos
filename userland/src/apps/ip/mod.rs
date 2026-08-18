//! `ip` — the network configuration command.
//!
//! The grammar lives in [`slopos_net_core::ip_plan`] and is decided before any
//! syscall runs, so it is host-testable; every state→string mapping comes from
//! [`slopos_net_core::render`], which the compositor's status indicator reads
//! too.
//!
//! Objects the grammar accepts but this kernel does not serve print
//! `ip: OBJECT: not supported by this kernel yet` and exit 1, never an empty
//! table. Nothing pre-checks `NET_ADMIN` — there is no syscall to read a task's
//! own flags, so the syscall is issued and its errno is rendered.

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

/// The two halves of the `ip: CONTEXT: MESSAGE` line plus the status it earns.
pub struct Failure {
    context: String,
    message: String,
    status: i32,
}

/// Exit status for a command that ran and failed.
const EXIT_RUNTIME: i32 = 1;
/// Exit status for a command line that is not a command.
const EXIT_USAGE: i32 = 2;

/// What the kernel says when an object is defined in the ABI and not yet served.
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
    /// `EPERM` names the binary because a system with no uids has no `sudo`:
    /// the privilege comes from the path the program was run from.
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

    pub fn unsupported(object: &str) -> Failure {
        Failure::runtime(object, NOT_SUPPORTED)
    }
}

pub type Outcome = Result<(), Failure>;

/// How long `ip monitor` runs when neither bound is given.
///
/// Bounded by default: a monitor that blocks forever is indistinguishable from
/// a hang in a serial transcript. `-t 0` asks for no deadline.
const DEFAULT_MONITOR_MS: i64 = 10_000;

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

/// `ifconfig` is this binary under another name — one program-registry entry
/// pointing at `/bin/ip` — rendering `ip addr show`.
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

    // Rejected here because the grammar never sees these bounds; without this
    // they would be accepted silently for an object that cannot use them.
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
/// Stripped before [`parse`] rather than added to the grammar: a bound on how
/// long a process runs is a property of this binary, not of the language.
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
                // The default deadline stays: the events may never arrive.
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

/// An argument as text for an error message.
fn text(token: &[u8]) -> String {
    match core::str::from_utf8(token) {
        Ok(s) => s.to_string(),
        Err(_) => String::from("<non-utf8>"),
    }
}

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
