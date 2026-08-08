//! The `ip` command grammar, parsed into a plan that performs no I/O.
//!
//! ```text
//! ip [-br|-brief] [-s|-stats] [-n|-numeric] OBJECT [COMMAND] [ARGS...]
//! ```
//!
//! Every user-visible decision `ip` makes is here and none of it needs a
//! kernel, which is what makes the grammar testable on the host: [`parse`]
//! turns bytes into a [`Plan`], and only the caller turns a [`Plan`] into
//! syscalls.
//!
//! Three rules shape the grammar.
//!
//! **Options precede the object.** `ip -br link show` is accepted and
//! `ip link show -br` is [`IpError::OptionAfterObject`]. iproute2 is lenient
//! here, but leniency makes "is this token an option or an operand?" depend on
//! everything parsed so far; the strict rule is stateless, so every token after
//! the object is an operand and there is nothing to get subtly wrong. Options
//! are multi-character words rather than single letters, so they do not bundle:
//! `-brs` is not `-br -s`.
//!
//! **Abbreviation applies to the object and the command, never to an operand.**
//! `ip a` is `ip addr` and `ip li sh` is `ip link show`, because those tokens
//! are drawn from a closed table this crate owns. A device name, an address or
//! a keyword operand is never abbreviated: those come from the outside world,
//! so a prefix match against them would mean the meaning of a command changes
//! when a new interface appears. The real collisions are kept rather than
//! broken by renaming: `d` is ambiguous between `dhcp` and `dns`, `n` and `ne`
//! between `neigh` and `net`, `s` between `show` and `set`, and each reports
//! the candidates. Renaming `net` to dodge the `neigh` collision would trade a
//! reported ambiguity for a name nobody would guess.
//!
//! **An omitted command means `show`**, except `ip dhcp`, which means
//! `ip dhcp status` — there is nothing to "show" about a client, and `status`
//! is what a person typing `ip dhcp` wants. A bare `ip` is [`IpError::Usage`].
//!
//! Tokens are `&[u8]` throughout: interface names are bytes in the ABI, so
//! keeping them bytes avoids a UTF-8 check that could reject a name the kernel
//! accepts, and it is what lets the parser live in a `no_std` crate.

use slopos_abi::net::{NET_IFNAMSIZ, NET_MAX_RESOLVERS};

use crate::addr::Ipv4;
use crate::argv::{TokenError, resolve_token};
use crate::cidr::Cidr;

/// The objects `ip` operates on, in table order.
pub const OBJECTS: [&str; 10] = [
    "addr", "dhcp", "dns", "help", "link", "monitor", "neigh", "net", "route", "status",
];

const LINK_COMMANDS: [&str; 3] = ["help", "set", "show"];
const ADDR_COMMANDS: [&str; 4] = ["add", "del", "flush", "show"];
const ROUTE_COMMANDS: [&str; 3] = ["add", "del", "show"];
const NEIGH_COMMANDS: [&str; 3] = ["del", "flush", "show"];
const DHCP_COMMANDS: [&str; 5] = ["release", "renew", "start", "status", "stop"];
const DNS_COMMANDS: [&str; 2] = ["set", "show"];
const STATUS_COMMANDS: [&str; 1] = ["show"];
/// `on`/`off` are the words a person reaches for and `enable`/`disable` are
/// the words a script written against another tool already has, so both are
/// accepted rather than one being canonical. They collide on `o`, which is
/// reported like any other ambiguity.
const NET_COMMANDS: [&str; 5] = ["disable", "enable", "off", "on", "show"];

/// Every word the grammar can print back at a person: object names, command
/// names, keyword operands, option spellings. A help renderer draws from this,
/// and the crate's glyph-coverage test holds all of it to what the console
/// font can draw.
pub const ALL_GRAMMAR_WORDS: &[&str] = &[
    "addr", "dhcp", "dns", "help", "link", "monitor", "neigh", "route", "status", "set", "show",
    "add", "del", "flush", "release", "renew", "start", "stop", "dev", "via", "default", "up",
    "down", "up|down", "net", "on", "off", "enable", "disable", "-br", "-brief", "-s", "-stats",
    "-n", "-numeric",
];

/// An `ip` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Object {
    Addr,
    Dhcp,
    Dns,
    Help,
    Link,
    Monitor,
    Neigh,
    /// The stack as a whole — the master networking switch.
    Net,
    Route,
    Status,
}

impl Object {
    /// The canonical name, as [`OBJECTS`] spells it.
    pub const fn name(self) -> &'static str {
        match self {
            Object::Addr => "addr",
            Object::Dhcp => "dhcp",
            Object::Dns => "dns",
            Object::Help => "help",
            Object::Link => "link",
            Object::Monitor => "monitor",
            Object::Neigh => "neigh",
            Object::Net => "net",
            Object::Route => "route",
            Object::Status => "status",
        }
    }

    /// The object's command table and the command an empty command position
    /// means, or `None` for an object that takes no command.
    const fn commands(self) -> Option<(&'static [&'static str], &'static str)> {
        match self {
            Object::Link => Some((&LINK_COMMANDS, "show")),
            Object::Addr => Some((&ADDR_COMMANDS, "show")),
            Object::Route => Some((&ROUTE_COMMANDS, "show")),
            Object::Neigh => Some((&NEIGH_COMMANDS, "show")),
            Object::Dhcp => Some((&DHCP_COMMANDS, "status")),
            Object::Dns => Some((&DNS_COMMANDS, "show")),
            Object::Net => Some((&NET_COMMANDS, "show")),
            Object::Status => Some((&STATUS_COMMANDS, "show")),
            Object::Help | Object::Monitor => None,
        }
    }

    fn from_name(name: &'static str) -> Object {
        match name {
            "addr" => Object::Addr,
            "dhcp" => Object::Dhcp,
            "dns" => Object::Dns,
            "help" => Object::Help,
            "link" => Object::Link,
            "monitor" => Object::Monitor,
            "neigh" => Object::Neigh,
            "net" => Object::Net,
            "route" => Object::Route,
            // `status` and anything a later table entry adds without a variant.
            _ => Object::Status,
        }
    }
}

/// The global options, which modify how a plan is rendered rather than what it
/// does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// `-br` / `-brief`: one fixed-width line per object.
    pub brief: bool,
    /// `-s` / `-stats`: include counters.
    pub stats: bool,
    /// `-n` / `-numeric`: never resolve a name.
    pub numeric: bool,
}

/// What a route entry names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteDest {
    /// The `default` keyword, i.e. `0.0.0.0/0`. Kept distinct from the prefix
    /// so the renderer can print back the word that was typed.
    Default,
    Prefix(Cidr),
}

/// A parsed command. Holding no file descriptors and performing no syscalls is
/// the point: the grammar is decided here and executed elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan<'a> {
    LinkShow {
        dev: Option<&'a [u8]>,
    },
    LinkSet {
        dev: &'a [u8],
        up: bool,
    },
    AddrShow {
        dev: Option<&'a [u8]>,
    },
    AddrAdd {
        cidr: Cidr,
        dev: &'a [u8],
    },
    AddrDel {
        cidr: Cidr,
        dev: &'a [u8],
    },
    AddrFlush {
        dev: Option<&'a [u8]>,
    },
    RouteShow {
        dev: Option<&'a [u8]>,
    },
    RouteAdd {
        dest: RouteDest,
        via: Ipv4,
        dev: &'a [u8],
    },
    RouteDel {
        dest: RouteDest,
        dev: Option<&'a [u8]>,
    },
    NeighShow {
        dev: Option<&'a [u8]>,
    },
    NeighDel {
        addr: Ipv4,
        dev: &'a [u8],
    },
    NeighFlush {
        dev: Option<&'a [u8]>,
    },
    DhcpStart {
        dev: &'a [u8],
    },
    DhcpStop {
        dev: &'a [u8],
    },
    DhcpRenew {
        dev: &'a [u8],
    },
    DhcpRelease {
        dev: &'a [u8],
    },
    DhcpStatus {
        dev: Option<&'a [u8]>,
    },
    DnsShow,
    DnsSet {
        servers: [Ipv4; NET_MAX_RESOLVERS],
        /// How many of `servers` were given; `1..=NET_MAX_RESOLVERS`.
        count: u8,
    },
    Monitor {
        filter: Option<&'a [u8]>,
    },
    /// Report the master networking switch.
    NetShow,
    /// Move the master networking switch.
    NetSet {
        enabled: bool,
    },
    Status,
    Help {
        /// `None` for `ip help`, `Some(obj)` for `ip OBJ help`.
        object: Option<Object>,
    },
}

/// A successfully parsed command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invocation<'a> {
    pub opts: Options,
    pub plan: Plan<'a>,
}

/// Why a command line is not a command.
///
/// The offending token travels with the error so a caller can name it, and the
/// ambiguous variants carry the table they were resolved against so the
/// message can list the candidates ([`crate::argv::matches`] produces them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpError<'a> {
    /// Nothing was asked for.
    Usage,
    UnknownObject {
        token: &'a [u8],
    },
    AmbiguousObject {
        token: &'a [u8],
        table: &'static [&'static str],
    },
    UnknownCommand {
        token: &'a [u8],
        object: Object,
    },
    AmbiguousCommand {
        token: &'a [u8],
        table: &'static [&'static str],
    },
    /// A keyword operand (`dev`, `via`, `up|down`) was expected here.
    MissingKeyword(&'static str),
    /// A required operand ran off the end of the line.
    MissingOperand,
    BadCidr {
        token: &'a [u8],
    },
    BadAddr {
        token: &'a [u8],
    },
    BadDevice {
        token: &'a [u8],
    },
    /// An option appeared after the object; options must precede it.
    OptionAfterObject {
        token: &'a [u8],
    },
    UnknownOption {
        token: &'a [u8],
    },
    /// Everything the grammar wanted was found, and there was more.
    TrailingOperand,
}

/// Parses an `ip` command line. `args` excludes `argv[0]`.
pub fn parse<'a>(args: &[&'a [u8]]) -> Result<Invocation<'a>, IpError<'a>> {
    let mut opts = Options::default();
    let mut idx = 0usize;
    while let Some(&arg) = args.get(idx) {
        if !is_option(arg) {
            break;
        }
        match arg {
            b"-br" | b"-brief" => opts.brief = true,
            b"-s" | b"-stats" => opts.stats = true,
            b"-n" | b"-numeric" => opts.numeric = true,
            _ => return Err(IpError::UnknownOption { token: arg }),
        }
        idx += 1;
    }

    let Some(&object_token) = args.get(idx) else {
        return Err(IpError::Usage);
    };
    let operands = &args[idx + 1..];
    for &token in operands {
        if is_option(token) {
            return Err(IpError::OptionAfterObject { token });
        }
    }

    let object = Object::from_name(resolve_token(object_token, &OBJECTS).map_err(
        |err| match err {
            TokenError::Unknown => IpError::UnknownObject {
                token: object_token,
            },
            TokenError::Ambiguous => IpError::AmbiguousObject {
                token: object_token,
                table: &OBJECTS,
            },
        },
    )?);

    let plan = match object.commands() {
        None => match object {
            Object::Help => {
                require_empty(operands)?;
                Plan::Help { object: None }
            }
            // `ip monitor [FILTER]`.
            _ => match operands {
                [] => Plan::Monitor { filter: None },
                [filter] => Plan::Monitor {
                    filter: Some(filter),
                },
                _ => return Err(IpError::TrailingOperand),
            },
        },
        Some((table, default_command)) => {
            let (command, rest) = match operands.split_first() {
                None => (default_command, operands),
                Some((&first, tail)) => {
                    let name = resolve_token(first, table).map_err(|err| match err {
                        TokenError::Unknown => IpError::UnknownCommand {
                            token: first,
                            object,
                        },
                        TokenError::Ambiguous => IpError::AmbiguousCommand {
                            token: first,
                            table,
                        },
                    })?;
                    (name, tail)
                }
            };
            dispatch(object, command, rest)?
        }
    };

    Ok(Invocation { opts, plan })
}

fn is_option(token: &[u8]) -> bool {
    token.len() > 1 && token[0] == b'-'
}

fn dispatch<'a>(
    object: Object,
    command: &'static str,
    operands: &[&'a [u8]],
) -> Result<Plan<'a>, IpError<'a>> {
    match (object, command) {
        (Object::Link, "show") => Ok(Plan::LinkShow {
            dev: optional_dev(operands)?,
        }),
        (Object::Link, "set") => parse_link_set(operands),
        (Object::Link, "help") => {
            require_empty(operands)?;
            Ok(Plan::Help {
                object: Some(Object::Link),
            })
        }

        (Object::Addr, "show") => Ok(Plan::AddrShow {
            dev: optional_dev(operands)?,
        }),
        (Object::Addr, "flush") => Ok(Plan::AddrFlush {
            dev: optional_dev(operands)?,
        }),
        (Object::Addr, "add") => {
            let (cidr, dev) = parse_cidr_dev(operands)?;
            Ok(Plan::AddrAdd { cidr, dev })
        }
        (Object::Addr, "del") => {
            let (cidr, dev) = parse_cidr_dev(operands)?;
            Ok(Plan::AddrDel { cidr, dev })
        }

        (Object::Route, "show") => Ok(Plan::RouteShow {
            dev: optional_dev(operands)?,
        }),
        (Object::Route, "add") => parse_route_add(operands),
        (Object::Route, "del") => {
            let (&dest_token, rest) = operands.split_first().ok_or(IpError::MissingOperand)?;
            Ok(Plan::RouteDel {
                dest: parse_dest(dest_token)?,
                dev: optional_dev(rest)?,
            })
        }

        (Object::Net, "show") => {
            require_empty(operands)?;
            Ok(Plan::NetShow)
        }
        (Object::Net, "on" | "enable") => {
            require_empty(operands)?;
            Ok(Plan::NetSet { enabled: true })
        }
        (Object::Net, "off" | "disable") => {
            require_empty(operands)?;
            Ok(Plan::NetSet { enabled: false })
        }

        (Object::Neigh, "show") => Ok(Plan::NeighShow {
            dev: optional_dev(operands)?,
        }),
        (Object::Neigh, "flush") => Ok(Plan::NeighFlush {
            dev: optional_dev(operands)?,
        }),
        (Object::Neigh, "del") => {
            let mut rest = Tokens::new(operands);
            let addr_token = rest.next().ok_or(IpError::MissingOperand)?;
            let addr =
                Ipv4::from_str_bytes(addr_token).ok_or(IpError::BadAddr { token: addr_token })?;
            let dev = check_dev(expect_keyword_value(&mut rest, "dev")?)?;
            require_end(&mut rest)?;
            Ok(Plan::NeighDel { addr, dev })
        }

        (Object::Dhcp, "start") => Ok(Plan::DhcpStart {
            dev: exactly_one_dev(operands)?,
        }),
        (Object::Dhcp, "stop") => Ok(Plan::DhcpStop {
            dev: exactly_one_dev(operands)?,
        }),
        (Object::Dhcp, "renew") => Ok(Plan::DhcpRenew {
            dev: exactly_one_dev(operands)?,
        }),
        (Object::Dhcp, "release") => Ok(Plan::DhcpRelease {
            dev: exactly_one_dev(operands)?,
        }),
        (Object::Dhcp, "status") => Ok(Plan::DhcpStatus {
            dev: optional_dev(operands)?,
        }),

        (Object::Dns, "show") => {
            require_empty(operands)?;
            Ok(Plan::DnsShow)
        }
        (Object::Dns, "set") => parse_dns_set(operands),

        (Object::Status, _) => {
            require_empty(operands)?;
            Ok(Plan::Status)
        }

        // Unreachable: `command` came from the table `object` names, and every
        // pair in those tables is handled above.
        _ => Err(IpError::Usage),
    }
}

/// `ip link set DEV up|down`. The device comes first: `ip link set up eth0`
/// reads `up` as the device and then finds no verb, which is the error the
/// wrong order deserves.
fn parse_link_set<'a>(operands: &[&'a [u8]]) -> Result<Plan<'a>, IpError<'a>> {
    let mut rest = Tokens::new(operands);
    let dev_token = rest.next().ok_or(IpError::MissingOperand)?;
    let dev = check_dev(dev_token)?;
    let verb = rest.next().ok_or(IpError::MissingOperand)?;
    let up = match verb {
        b"up" => true,
        b"down" => false,
        _ => return Err(IpError::MissingKeyword("up|down")),
    };
    require_end(&mut rest)?;
    Ok(Plan::LinkSet { dev, up })
}

/// `CIDR dev DEV`, shared by `addr add` and `addr del`.
fn parse_cidr_dev<'a>(operands: &[&'a [u8]]) -> Result<(Cidr, &'a [u8]), IpError<'a>> {
    let mut rest = Tokens::new(operands);
    let cidr_token = rest.next().ok_or(IpError::MissingOperand)?;
    let cidr = Cidr::from_str_bytes(cidr_token).ok_or(IpError::BadCidr { token: cidr_token })?;
    let dev = check_dev(expect_keyword_value(&mut rest, "dev")?)?;
    require_end(&mut rest)?;
    Ok((cidr, dev))
}

/// `default|CIDR via IP dev DEV`. `via` and `dev` are both required: a route
/// with neither is not a route, and guessing either from the address would be
/// a guess the routing table then acts on.
fn parse_route_add<'a>(operands: &[&'a [u8]]) -> Result<Plan<'a>, IpError<'a>> {
    let mut rest = Tokens::new(operands);
    let dest_token = rest.next().ok_or(IpError::MissingOperand)?;
    let dest = parse_dest(dest_token)?;
    let via_token = expect_keyword_value(&mut rest, "via")?;
    let via = Ipv4::from_str_bytes(via_token).ok_or(IpError::BadAddr { token: via_token })?;
    let dev = check_dev(expect_keyword_value(&mut rest, "dev")?)?;
    require_end(&mut rest)?;
    Ok(Plan::RouteAdd { dest, via, dev })
}

fn parse_dns_set<'a>(operands: &[&'a [u8]]) -> Result<Plan<'a>, IpError<'a>> {
    if operands.is_empty() {
        return Err(IpError::MissingOperand);
    }
    if operands.len() > NET_MAX_RESOLVERS {
        return Err(IpError::TrailingOperand);
    }
    let mut servers = [Ipv4::UNSPECIFIED; NET_MAX_RESOLVERS];
    for (slot, &token) in servers.iter_mut().zip(operands) {
        *slot = Ipv4::from_str_bytes(token).ok_or(IpError::BadAddr { token })?;
    }
    Ok(Plan::DnsSet {
        servers,
        count: operands.len() as u8,
    })
}

fn parse_dest(token: &[u8]) -> Result<RouteDest, IpError<'_>> {
    if token == b"default" {
        Ok(RouteDest::Default)
    } else {
        Cidr::from_str_bytes(token)
            .map(RouteDest::Prefix)
            .ok_or(IpError::BadCidr { token })
    }
}

/// A trailing `[dev DEV]` or bare `[DEV]`, as every `show` and `flush` takes.
/// Both spellings are accepted because `ip addr show dev eth0` is the form
/// muscle memory produces and `ip addr show eth0` is the form that is shorter;
/// an interface actually named `dev` is reachable as the one-token form.
fn optional_dev<'a>(operands: &[&'a [u8]]) -> Result<Option<&'a [u8]>, IpError<'a>> {
    match operands {
        [] => Ok(None),
        [dev] => Ok(Some(check_dev(dev)?)),
        [keyword, dev] if *keyword == b"dev" => Ok(Some(check_dev(dev)?)),
        [_, _] => Err(IpError::MissingKeyword("dev")),
        _ => Err(IpError::TrailingOperand),
    }
}

/// One bare device name, as the `dhcp` verbs take.
fn exactly_one_dev<'a>(operands: &[&'a [u8]]) -> Result<&'a [u8], IpError<'a>> {
    match operands {
        [] => Err(IpError::MissingOperand),
        [dev] => check_dev(dev),
        _ => Err(IpError::TrailingOperand),
    }
}

/// A cursor over one command's operand tokens.
///
/// A named type rather than an `impl Iterator<Item = &'a [u8]>` bound: check 8
/// of `scripts/check_task_ownership.sh` parses the generic list, the argument
/// list and the return type but deliberately not the `where` clause, so an
/// associated-type equality bound reads to it as a lifetime the caller may pick
/// freely. That over-report has no exemption mechanism, so `'a` has to be
/// visible in the argument list itself.
///
/// Two lifetimes rather than one: `'t` is the operand slice, which lives on
/// `parse`'s stack, and `'a` is the token bytes, which outlive the parse and
/// travel out in the [`Plan`].
struct Tokens<'t, 'a> {
    rest: &'t [&'a [u8]],
}

impl<'t, 'a> Tokens<'t, 'a> {
    fn new(operands: &'t [&'a [u8]]) -> Tokens<'t, 'a> {
        Tokens { rest: operands }
    }

    /// Take the next token, or `None` at the end.
    fn next(&mut self) -> Option<&'a [u8]> {
        let (first, rest) = self.rest.split_first()?;
        self.rest = rest;
        Some(first)
    }
}

/// Consume `KEYWORD VALUE` and return the value.
fn expect_keyword_value<'a>(
    rest: &mut Tokens<'_, 'a>,
    keyword: &'static str,
) -> Result<&'a [u8], IpError<'a>> {
    match rest.next() {
        Some(token) if token == keyword.as_bytes() => rest.next().ok_or(IpError::MissingOperand),
        _ => Err(IpError::MissingKeyword(keyword)),
    }
}

/// Everything the grammar wanted was found; anything left over is an error.
fn require_end<'a>(rest: &mut Tokens<'_, 'a>) -> Result<(), IpError<'a>> {
    if rest.next().is_some() {
        Err(IpError::TrailingOperand)
    } else {
        Ok(())
    }
}

fn require_empty<'a>(operands: &[&'a [u8]]) -> Result<(), IpError<'a>> {
    if operands.is_empty() {
        Ok(())
    } else {
        Err(IpError::TrailingOperand)
    }
}

/// An interface name must fit the ABI's `[u8; NET_IFNAMSIZ]` field and contain
/// nothing that would confuse a renderer or a path: printable, no space, no
/// slash.
fn check_dev(name: &[u8]) -> Result<&[u8], IpError<'_>> {
    if name.is_empty() || name.len() > NET_IFNAMSIZ {
        return Err(IpError::BadDevice { token: name });
    }
    for &byte in name {
        if !byte.is_ascii_graphic() || byte == b'/' {
            return Err(IpError::BadDevice { token: name });
        }
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<'a>(line: &[&'a [u8]]) -> Result<Invocation<'a>, IpError<'a>> {
        parse(line)
    }

    fn plan<'a>(line: &[&'a [u8]]) -> Plan<'a> {
        run(line).expect("expected a plan").plan
    }

    fn err<'a>(line: &[&'a [u8]]) -> IpError<'a> {
        run(line).expect_err("expected an error")
    }

    // -- objects, commands, abbreviation ------------------------------------

    #[test]
    fn bare_ip_is_usage() {
        assert_eq!(err(&[]), IpError::Usage);
        assert_eq!(err(&[b"-br"]), IpError::Usage);
    }

    #[test]
    fn addr_abbreviations_all_mean_addr_show() {
        let expected = Plan::AddrShow { dev: None };
        assert_eq!(plan(&[b"a"]), expected);
        assert_eq!(plan(&[b"addr"]), expected);
        assert_eq!(plan(&[b"addr", b"show"]), expected);
        assert_eq!(plan(&[b"ad", b"sh"]), expected);
    }

    #[test]
    fn link_show_is_the_default_command() {
        assert_eq!(plan(&[b"link"]), Plan::LinkShow { dev: None });
        assert_eq!(plan(&[b"l"]), Plan::LinkShow { dev: None });
        assert_eq!(
            plan(&[b"link", b"show", b"eth0"]),
            Plan::LinkShow { dev: Some(b"eth0") }
        );
        assert_eq!(
            plan(&[b"link", b"show", b"dev", b"eth0"]),
            Plan::LinkShow { dev: Some(b"eth0") }
        );
    }

    #[test]
    fn dhcp_defaults_to_status_not_show() {
        assert_eq!(plan(&[b"dhcp"]), Plan::DhcpStatus { dev: None });
        assert_eq!(
            plan(&[b"dhcp", b"status", b"eth0"]),
            Plan::DhcpStatus { dev: Some(b"eth0") }
        );
    }

    /// `ip net` reaches the master switch, in both vocabularies.
    ///
    /// `on`/`off` and `enable`/`disable` are the same plan rather than two,
    /// because a switch has two positions and a caller should not have to know
    /// which word this tool prefers.
    #[test]
    fn net_switch_accepts_both_vocabularies() {
        assert_eq!(plan(&[b"net", b"on"]), Plan::NetSet { enabled: true });
        assert_eq!(plan(&[b"net", b"enable"]), Plan::NetSet { enabled: true });
        assert_eq!(plan(&[b"net", b"off"]), Plan::NetSet { enabled: false });
        assert_eq!(plan(&[b"net", b"disable"]), Plan::NetSet { enabled: false });
        // Command abbreviation still applies; only `o` is ambiguous.
        assert_eq!(plan(&[b"net", b"en"]), Plan::NetSet { enabled: true });
        assert_eq!(plan(&[b"net", b"dis"]), Plan::NetSet { enabled: false });
    }

    #[test]
    fn net_show_is_the_default_command() {
        assert_eq!(plan(&[b"net"]), Plan::NetShow);
        assert_eq!(plan(&[b"net", b"show"]), Plan::NetShow);
        assert_eq!(plan(&[b"net", b"s"]), Plan::NetShow);
    }

    /// The switch takes no operand at all: it addresses the stack, not an
    /// interface, so `ip net on eth0` is a request the ABI cannot express and
    /// must be refused rather than quietly ignoring the device.
    #[test]
    fn net_takes_no_operand() {
        assert_eq!(err(&[b"net", b"on", b"eth0"]), IpError::TrailingOperand);
        assert_eq!(err(&[b"net", b"show", b"eth0"]), IpError::TrailingOperand);
    }

    #[test]
    fn net_and_neigh_collide_on_their_shared_prefix() {
        for token in [b"n".as_slice(), b"ne".as_slice()] {
            let error = err(&[token]);
            let IpError::AmbiguousObject { token: got, table } = error else {
                panic!("expected AmbiguousObject for {token:?}, got {error:?}");
            };
            assert_eq!(got, token);
            let candidates: [&str; 2] = {
                let mut it = crate::argv::matches(token, table);
                [it.next().unwrap(), it.next().unwrap()]
            };
            assert_eq!(candidates, ["neigh", "net"]);
            assert_eq!(crate::argv::matches(token, table).count(), 2);
        }
    }

    /// One more character each way disambiguates, and `net` resolves by exact
    /// match despite `neigh` sharing its first two letters.
    #[test]
    fn one_more_character_resolves_the_collision() {
        assert_eq!(plan(&[b"nei"]), Plan::NeighShow { dev: None });
        assert_eq!(plan(&[b"neigh"]), Plan::NeighShow { dev: None });
        assert_eq!(plan(&[b"net"]), Plan::NetShow);
    }

    /// Every object's shortest abbreviation resolves to the object it names,
    /// and a collision is reported rather than silently bound to one candidate.
    #[test]
    fn existing_abbreviations_are_unmoved() {
        assert_eq!(plan(&[b"a"]), Plan::AddrShow { dev: None });
        assert_eq!(plan(&[b"l"]), Plan::LinkShow { dev: None });
        assert_eq!(plan(&[b"m"]), Plan::Monitor { filter: None });
        assert_eq!(plan(&[b"r"]), Plan::RouteShow { dev: None });
        assert_eq!(plan(&[b"st"]), Plan::Status);
        assert_eq!(plan(&[b"h"]), Plan::Help { object: None });
        assert_eq!(
            err(&[b"d"]),
            IpError::AmbiguousObject {
                token: b"d",
                table: &OBJECTS,
            }
        );
        // `s` prefixes only `status` among the objects; the `show`/`set`
        // collision the module doc names lives in the command tables.
        assert_eq!(plan(&[b"s"]), Plan::Status);
    }

    #[test]
    fn unknown_object_is_reported_with_its_token() {
        assert_eq!(err(&[b"zebra"]), IpError::UnknownObject { token: b"zebra" });
    }

    #[test]
    fn ambiguous_object_lists_candidates() {
        let error = err(&[b"d"]);
        let IpError::AmbiguousObject { token, table } = error else {
            panic!("expected AmbiguousObject, got {error:?}");
        };
        assert_eq!(token, b"d");
        let candidates: [&str; 2] = {
            let mut it = crate::argv::matches(token, table);
            [it.next().unwrap(), it.next().unwrap()]
        };
        assert_eq!(candidates, ["dhcp", "dns"]);
        assert_eq!(crate::argv::matches(token, table).count(), 2);
    }

    #[test]
    fn ambiguous_command_lists_candidates() {
        let error = err(&[b"dns", b"s"]);
        let IpError::AmbiguousCommand { token, table } = error else {
            panic!("expected AmbiguousCommand, got {error:?}");
        };
        assert_eq!(token, b"s");
        assert_eq!(crate::argv::matches(token, table).count(), 2);
        // `link` has the same collision.
        assert!(matches!(
            err(&[b"link", b"s"]),
            IpError::AmbiguousCommand { .. }
        ));
        // And `dhcp` collides on `st` across start/status/stop.
        assert!(matches!(
            err(&[b"dhcp", b"st", b"eth0"]),
            IpError::AmbiguousCommand { .. }
        ));
    }

    #[test]
    fn unique_command_prefixes_resolve() {
        assert_eq!(
            plan(&[b"dhcp", b"ren", b"eth0"]),
            Plan::DhcpRenew { dev: b"eth0" }
        );
        assert_eq!(
            plan(&[b"dhcp", b"rel", b"eth0"]),
            Plan::DhcpRelease { dev: b"eth0" }
        );
        assert_eq!(
            plan(&[b"dhcp", b"sto", b"eth0"]),
            Plan::DhcpStop { dev: b"eth0" }
        );
    }

    #[test]
    fn unknown_command_names_its_object() {
        assert_eq!(
            err(&[b"link", b"wiggle"]),
            IpError::UnknownCommand {
                token: b"wiggle",
                object: Object::Link
            }
        );
        // An operand where a command belongs is an unknown command, not a
        // device: `ip link eth0` never meant `ip link show eth0`.
        assert!(matches!(
            err(&[b"link", b"eth0"]),
            IpError::UnknownCommand { .. }
        ));
    }

    // -- options -------------------------------------------------------------

    #[test]
    fn options_precede_the_object() {
        let parsed = run(&[b"-br", b"l"]).unwrap();
        assert!(parsed.opts.brief);
        assert_eq!(parsed.plan, Plan::LinkShow { dev: None });

        let parsed = run(&[b"-s", b"link", b"show", b"eth0"]).unwrap();
        assert!(parsed.opts.stats);
        assert_eq!(parsed.plan, Plan::LinkShow { dev: Some(b"eth0") });

        let parsed = run(&[b"-br", b"-s", b"-n", b"addr"]).unwrap();
        assert_eq!(
            parsed.opts,
            Options {
                brief: true,
                stats: true,
                numeric: true
            }
        );
    }

    #[test]
    fn long_option_spellings_are_equivalent() {
        assert_eq!(
            run(&[b"-brief", b"link"]).unwrap().opts,
            run(&[b"-br", b"link"]).unwrap().opts
        );
        assert_eq!(
            run(&[b"-stats", b"link"]).unwrap().opts,
            run(&[b"-s", b"link"]).unwrap().opts
        );
        assert_eq!(
            run(&[b"-numeric", b"link"]).unwrap().opts,
            run(&[b"-n", b"link"]).unwrap().opts
        );
    }

    #[test]
    fn option_after_object_is_rejected() {
        assert_eq!(
            err(&[b"link", b"show", b"-br"]),
            IpError::OptionAfterObject { token: b"-br" }
        );
        assert_eq!(
            err(&[b"link", b"-s", b"show"]),
            IpError::OptionAfterObject { token: b"-s" }
        );
    }

    #[test]
    fn unknown_option_is_rejected_before_the_object() {
        assert_eq!(
            err(&[b"-z", b"link"]),
            IpError::UnknownOption { token: b"-z" }
        );
        // Options do not bundle: `-brs` is not `-br -s`.
        assert_eq!(
            err(&[b"-brs", b"link"]),
            IpError::UnknownOption { token: b"-brs" }
        );
    }

    // -- link ----------------------------------------------------------------

    #[test]
    fn link_set_up_and_down() {
        assert_eq!(
            plan(&[b"link", b"set", b"eth0", b"up"]),
            Plan::LinkSet {
                dev: b"eth0",
                up: true
            }
        );
        assert_eq!(
            plan(&[b"l", b"se", b"eth0", b"down"]),
            Plan::LinkSet {
                dev: b"eth0",
                up: false
            }
        );
    }

    #[test]
    fn link_set_without_a_verb_is_incomplete() {
        assert_eq!(err(&[b"link", b"set", b"eth0"]), IpError::MissingOperand);
        assert_eq!(err(&[b"link", b"set"]), IpError::MissingOperand);
    }

    #[test]
    fn link_set_in_the_wrong_order_is_rejected() {
        assert_eq!(
            err(&[b"link", b"set", b"up", b"eth0"]),
            IpError::MissingKeyword("up|down")
        );
    }

    #[test]
    fn link_set_rejects_a_trailing_operand() {
        assert_eq!(
            err(&[b"link", b"set", b"eth0", b"up", b"now"]),
            IpError::TrailingOperand
        );
    }

    #[test]
    fn link_help_is_object_scoped_help() {
        assert_eq!(
            plan(&[b"link", b"help"]),
            Plan::Help {
                object: Some(Object::Link)
            }
        );
        assert_eq!(plan(&[b"help"]), Plan::Help { object: None });
        assert_eq!(err(&[b"help", b"link"]), IpError::TrailingOperand);
    }

    // -- addr ----------------------------------------------------------------

    #[test]
    fn addr_add_and_del() {
        let cidr = Cidr::from_str_bytes(b"10.0.2.15/24").unwrap();
        assert_eq!(
            plan(&[b"addr", b"add", b"10.0.2.15/24", b"dev", b"eth0"]),
            Plan::AddrAdd { cidr, dev: b"eth0" }
        );
        assert_eq!(
            plan(&[b"a", b"d", b"10.0.2.15/24", b"dev", b"eth0"]),
            Plan::AddrDel { cidr, dev: b"eth0" }
        );
    }

    #[test]
    fn addr_add_bare_address_is_a_host_route() {
        assert_eq!(
            plan(&[b"addr", b"add", b"10.0.2.15", b"dev", b"eth0"]),
            Plan::AddrAdd {
                cidr: Cidr::from_str_bytes(b"10.0.2.15/32").unwrap(),
                dev: b"eth0"
            }
        );
    }

    #[test]
    fn addr_add_reports_a_bad_prefix() {
        assert_eq!(
            err(&[b"addr", b"add", b"10.0.2.15/33", b"dev", b"eth0"]),
            IpError::BadCidr {
                token: b"10.0.2.15/33"
            }
        );
        assert_eq!(
            err(&[b"addr", b"add", b"10.0.2.256/24", b"dev", b"eth0"]),
            IpError::BadCidr {
                token: b"10.0.2.256/24"
            }
        );
    }

    #[test]
    fn addr_add_requires_the_dev_keyword() {
        assert_eq!(
            err(&[b"addr", b"add", b"10.0.2.15/24"]),
            IpError::MissingKeyword("dev")
        );
        assert_eq!(
            err(&[b"addr", b"add", b"10.0.2.15/24", b"eth0"]),
            IpError::MissingKeyword("dev")
        );
        assert_eq!(
            err(&[b"addr", b"add", b"10.0.2.15/24", b"dev"]),
            IpError::MissingOperand
        );
        assert_eq!(err(&[b"addr", b"add"]), IpError::MissingOperand);
    }

    #[test]
    fn addr_flush_takes_an_optional_device() {
        assert_eq!(plan(&[b"addr", b"flush"]), Plan::AddrFlush { dev: None });
        assert_eq!(
            plan(&[b"addr", b"f", b"dev", b"eth0"]),
            Plan::AddrFlush { dev: Some(b"eth0") }
        );
    }

    // -- route ---------------------------------------------------------------

    #[test]
    fn route_add_default() {
        assert_eq!(
            plan(&[
                b"route",
                b"add",
                b"default",
                b"via",
                b"10.0.2.2",
                b"dev",
                b"eth0"
            ]),
            Plan::RouteAdd {
                dest: RouteDest::Default,
                via: Ipv4([10, 0, 2, 2]),
                dev: b"eth0"
            }
        );
    }

    #[test]
    fn route_add_prefix() {
        assert_eq!(
            plan(&[
                b"r",
                b"a",
                b"192.168.9.0/24",
                b"via",
                b"10.0.2.2",
                b"dev",
                b"eth0"
            ]),
            Plan::RouteAdd {
                dest: RouteDest::Prefix(Cidr::from_str_bytes(b"192.168.9.0/24").unwrap()),
                via: Ipv4([10, 0, 2, 2]),
                dev: b"eth0"
            }
        );
    }

    #[test]
    fn route_add_reports_each_missing_piece() {
        assert_eq!(err(&[b"route", b"add"]), IpError::MissingOperand);
        assert_eq!(
            err(&[b"route", b"add", b"default"]),
            IpError::MissingKeyword("via")
        );
        assert_eq!(
            err(&[b"route", b"add", b"default", b"via"]),
            IpError::MissingOperand
        );
        assert_eq!(
            err(&[b"route", b"add", b"default", b"via", b"10.0.2.2"]),
            IpError::MissingKeyword("dev")
        );
        assert_eq!(
            err(&[b"route", b"add", b"default", b"10.0.2.2", b"dev", b"eth0"]),
            IpError::MissingKeyword("via")
        );
    }

    #[test]
    fn route_add_reports_a_bad_gateway() {
        assert_eq!(
            err(&[
                b"route", b"add", b"default", b"via", b"10.0.2", b"dev", b"eth0"
            ]),
            IpError::BadAddr { token: b"10.0.2" }
        );
        assert_eq!(
            err(&[
                b"route",
                b"add",
                b"notanet",
                b"via",
                b"10.0.2.2",
                b"dev",
                b"eth0"
            ]),
            IpError::BadCidr { token: b"notanet" }
        );
    }

    #[test]
    fn route_show_and_del() {
        assert_eq!(plan(&[b"route"]), Plan::RouteShow { dev: None });
        assert_eq!(
            plan(&[b"route", b"del", b"default"]),
            Plan::RouteDel {
                dest: RouteDest::Default,
                dev: None
            }
        );
        assert_eq!(
            plan(&[b"route", b"del", b"192.168.9.0/24", b"dev", b"eth0"]),
            Plan::RouteDel {
                dest: RouteDest::Prefix(Cidr::from_str_bytes(b"192.168.9.0/24").unwrap()),
                dev: Some(b"eth0")
            }
        );
        assert_eq!(err(&[b"route", b"del"]), IpError::MissingOperand);
    }

    // -- neigh ---------------------------------------------------------------

    #[test]
    fn neigh_show_del_and_flush() {
        assert_eq!(plan(&[b"neigh"]), Plan::NeighShow { dev: None });
        assert_eq!(
            plan(&[b"nei", b"del", b"10.0.2.3", b"dev", b"eth0"]),
            Plan::NeighDel {
                addr: Ipv4([10, 0, 2, 3]),
                dev: b"eth0"
            }
        );
        assert_eq!(
            plan(&[b"neigh", b"flush", b"dev", b"eth0"]),
            Plan::NeighFlush { dev: Some(b"eth0") }
        );
    }

    #[test]
    fn neigh_del_validates_its_address() {
        assert_eq!(
            err(&[b"neigh", b"del", b"10.0.2.999", b"dev", b"eth0"]),
            IpError::BadAddr {
                token: b"10.0.2.999"
            }
        );
        assert_eq!(
            err(&[b"neigh", b"del", b"10.0.2.3"]),
            IpError::MissingKeyword("dev")
        );
    }

    // -- dhcp ----------------------------------------------------------------

    #[test]
    fn dhcp_verbs_take_a_bare_device() {
        assert_eq!(
            plan(&[b"dhcp", b"start", b"eth0"]),
            Plan::DhcpStart { dev: b"eth0" }
        );
        assert_eq!(
            plan(&[b"dhcp", b"renew", b"eth0"]),
            Plan::DhcpRenew { dev: b"eth0" }
        );
        assert_eq!(err(&[b"dhcp", b"renew"]), IpError::MissingOperand);
        assert_eq!(
            err(&[b"dhcp", b"renew", b"eth0", b"eth1"]),
            IpError::TrailingOperand
        );
    }

    // -- dns -----------------------------------------------------------------

    #[test]
    fn dns_show_and_set() {
        assert_eq!(plan(&[b"dns"]), Plan::DnsShow);
        assert_eq!(plan(&[b"dns", b"sh"]), Plan::DnsShow);
        assert_eq!(
            plan(&[b"dns", b"set", b"1.1.1.1"]),
            Plan::DnsSet {
                servers: [Ipv4([1, 1, 1, 1]), Ipv4::UNSPECIFIED, Ipv4::UNSPECIFIED],
                count: 1
            }
        );
        assert_eq!(
            plan(&[b"dns", b"set", b"1.1.1.1", b"8.8.8.8", b"9.9.9.9"]),
            Plan::DnsSet {
                servers: [Ipv4([1, 1, 1, 1]), Ipv4([8, 8, 8, 8]), Ipv4([9, 9, 9, 9])],
                count: 3
            }
        );
    }

    #[test]
    fn dns_set_bounds_and_validates() {
        assert_eq!(err(&[b"dns", b"set"]), IpError::MissingOperand);
        assert_eq!(
            err(&[
                b"dns", b"set", b"1.1.1.1", b"8.8.8.8", b"9.9.9.9", b"4.4.4.4"
            ]),
            IpError::TrailingOperand
        );
        assert_eq!(
            err(&[b"dns", b"set", b"1.1.1.1", b"nope"]),
            IpError::BadAddr { token: b"nope" }
        );
        assert_eq!(err(&[b"dns", b"show", b"extra"]), IpError::TrailingOperand);
    }

    // -- monitor and status --------------------------------------------------

    #[test]
    fn monitor_takes_an_optional_filter() {
        assert_eq!(plan(&[b"monitor"]), Plan::Monitor { filter: None });
        assert_eq!(
            plan(&[b"m", b"link"]),
            Plan::Monitor {
                filter: Some(b"link")
            }
        );
        assert_eq!(
            err(&[b"monitor", b"link", b"addr"]),
            IpError::TrailingOperand
        );
    }

    #[test]
    fn status_takes_nothing() {
        assert_eq!(plan(&[b"status"]), Plan::Status);
        assert_eq!(plan(&[b"s"]), Plan::Status);
        assert_eq!(plan(&[b"status", b"show"]), Plan::Status);
        assert_eq!(
            err(&[b"status", b"eth0"]),
            IpError::UnknownCommand {
                token: b"eth0",
                object: Object::Status
            }
        );
    }

    // -- device-name validation ----------------------------------------------

    #[test]
    fn device_names_are_validated() {
        assert_eq!(
            err(&[b"link", b"show", b"dev", b""]),
            IpError::BadDevice { token: b"" }
        );
        assert_eq!(
            err(&[b"link", b"set", b"eth 0", b"up"]),
            IpError::BadDevice { token: b"eth 0" }
        );
        assert_eq!(
            err(&[b"link", b"set", b"../etc", b"up"]),
            IpError::BadDevice { token: b"../etc" }
        );
        // NET_IFNAMSIZ bytes fit; one more does not.
        let exact: &[u8] = b"0123456789abcdef";
        assert_eq!(exact.len(), NET_IFNAMSIZ);
        assert_eq!(
            plan(&[b"link", b"show", exact]),
            Plan::LinkShow { dev: Some(exact) }
        );
        let over: &[u8] = b"0123456789abcdefg";
        assert_eq!(
            err(&[b"link", b"show", over]),
            IpError::BadDevice { token: over }
        );
    }

    #[test]
    fn objects_and_commands_are_never_abbreviated_in_operand_position() {
        // `up` is a keyword, not a prefix-matched token: `u` is not `up`.
        assert_eq!(
            err(&[b"link", b"set", b"eth0", b"u"]),
            IpError::MissingKeyword("up|down")
        );
        // `dev` likewise.
        assert_eq!(
            err(&[b"addr", b"add", b"10.0.0.1/24", b"d", b"eth0"]),
            IpError::MissingKeyword("dev")
        );
        // And `default` is spelled out.
        assert_eq!(
            err(&[b"route", b"del", b"def"]),
            IpError::BadCidr { token: b"def" }
        );
    }

    #[test]
    fn object_table_and_grammar_words_agree() {
        for object in OBJECTS {
            assert!(
                ALL_GRAMMAR_WORDS.contains(&object),
                "{object} is missing from ALL_GRAMMAR_WORDS"
            );
        }
        for table in [
            &LINK_COMMANDS[..],
            &ADDR_COMMANDS[..],
            &ROUTE_COMMANDS[..],
            &NEIGH_COMMANDS[..],
            &DHCP_COMMANDS[..],
            &DNS_COMMANDS[..],
            &STATUS_COMMANDS[..],
        ] {
            for command in table {
                assert!(
                    ALL_GRAMMAR_WORDS.contains(command),
                    "{command} is missing from ALL_GRAMMAR_WORDS"
                );
            }
        }
    }

    #[test]
    fn every_object_name_round_trips() {
        for name in OBJECTS {
            let object = Object::from_name(name);
            assert_eq!(object.name(), name);
        }
    }
}
