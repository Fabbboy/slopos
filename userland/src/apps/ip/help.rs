//! `ip help` and `ip OBJECT help`.
//!
//! The text names the same objects, commands and options
//! [`slopos_net_core::ip_plan`] parses, and every word it prints is in that
//! module's `ALL_GRAMMAR_WORDS`, which the crate's own glyph-coverage test
//! holds to what the console font can draw.
//!
//! Where an object is defined in the ABI and not served by this kernel, the
//! help says so. A person reading a usage line should not have to run a command
//! to find out it cannot work.

use slopos_net_core::ip_plan::Object;

/// The one-line summary, printed on stdout for `ip help` and alongside the
/// error for a command line with no object.
pub fn usage() {
    println!("usage: ip [-br|-brief] [-s|-stats] [-n|-numeric] OBJECT [COMMAND] [ARGS...]");
    println!("       options must precede OBJECT; OBJECT and COMMAND may be abbreviated");
}

pub fn print(object: Option<Object>) {
    match object {
        None => print_overview(),
        Some(Object::Link) => print_link(),
        Some(Object::Addr) => print_addr(),
        Some(Object::Route) => print_route(),
        Some(Object::Neigh) => print_neigh(),
        Some(Object::Dhcp) => print_dhcp(),
        Some(Object::Dns) => print_dns(),
        Some(Object::Net) => print_net(),
        Some(Object::Monitor) => print_monitor(),
        Some(Object::Status) => print_status(),
        Some(Object::Help) => print_overview(),
    }
}

fn print_overview() {
    usage();
    println!();
    println!("objects:");
    println!("  link      interfaces, their flags and counters");
    println!("  addr      addresses assigned to interfaces");
    println!("  route     the routing table");
    println!("  neigh     the neighbour cache");
    println!("  dhcp      the DHCP client");
    println!("  dns       resolver configuration");
    println!("  net       the master networking switch");
    println!("  monitor   stream configuration changes as they happen");
    println!("  status    the whole stack in one screen");
    println!("  help      this text");
    println!();
    println!("`ip OBJECT help` describes one object. An omitted COMMAND means");
    println!("`show`, except `ip dhcp`, which means `ip dhcp status`.");
    println!();
    println!("This kernel does not serve `neigh show`, `dns`, `dhcp`, or the");
    println!("address and route mutators; those report it rather than printing");
    println!("an empty table.");
}

fn print_link() {
    println!("usage: ip link show [dev DEVICE]");
    println!("       ip link set dev DEVICE up|down");
    println!();
    println!("  -br   one fixed-width line per interface");
    println!("  -s    include RX/TX counters");
    println!();
    println!("`set` needs NET_ADMIN, which /bin/ip is granted by its path.");
    println!("Loopback reports state UNKNOWN: it has no link layer to have an");
    println!("operational state about.");
}

fn print_addr() {
    println!("usage: ip addr show [dev DEVICE]");
    println!("       ip addr add ADDR/LEN dev DEVICE     (not served by this kernel)");
    println!("       ip addr del ADDR/LEN dev DEVICE     (not served by this kernel)");
    println!("       ip addr flush [dev DEVICE]");
    println!();
    println!("  -br   one fixed-width line per interface");
    println!();
    println!("A bare address means /32: no classful prefix is inferred.");
}

fn print_route() {
    println!("usage: ip route show [dev DEVICE]");
    println!("       ip route add default|PREFIX/LEN via ADDR dev DEVICE");
    println!("       ip route del default|PREFIX/LEN [dev DEVICE]");
    println!();
    println!("`add` and `del` are not served by this kernel yet.");
}

fn print_neigh() {
    println!("usage: ip neigh show [dev DEVICE]        (not served by this kernel)");
    println!("       ip neigh del ADDR dev DEVICE");
    println!("       ip neigh flush [dev DEVICE]");
}

fn print_dhcp() {
    println!("usage: ip dhcp status [dev DEVICE]");
    println!("       ip dhcp start|stop|renew|release dev DEVICE");
    println!();
    println!("This kernel has no DHCP client; every verb reports that.");
}

fn print_dns() {
    println!("usage: ip dns show");
    println!("       ip dns set ADDR [ADDR [ADDR]]");
    println!();
    println!("Neither is served by this kernel yet.");
}

fn print_net() {
    println!("usage: ip net show");
    println!("       ip net on|off        (also spelled enable|disable)");
    println!();
    println!("The switch addresses the whole stack, not one device: turning it");
    println!("off downs every interface. `on`/`off` need NET_ADMIN, which");
    println!("/bin/ip is granted by its path.");
}

fn print_monitor() {
    println!("usage: ip [-c COUNT] [-t MILLISECONDS] monitor [FILTER]");
    println!();
    println!("FILTER is one of: link, addr, route, dns, conn, dhcp, global,");
    println!("neigh, all. The default set omits neigh, because ARP churn would");
    println!("keep the event ring in permanent overflow.");
    println!();
    println!("Stops after 10 seconds unless -t says otherwise; -t 0 runs until");
    println!("interrupted. -c stops after COUNT events.");
}

fn print_status() {
    println!("usage: ip status");
    println!();
    println!("Renders the same whole-stack record the compositor's network");
    println!("indicator reads, through the same renderer.");
}
