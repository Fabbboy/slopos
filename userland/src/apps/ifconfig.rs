use crate::syscall::{UserNetInfo, core, net::net_info, tty};

fn format_ipv4(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

pub fn ifconfig_main() -> ! {
    let mut info = UserNetInfo::default();
    if net_info(&mut info) != 0 {
        let _ = tty::write(b"ifconfig: net_info syscall failed\n");
        core::exit_with_code(1);
    }

    if info.nic_ready == 0 {
        let _ = tty::write(b"ifconfig: no network interface\n");
        core::exit_with_code(1);
    }

    let flags = if info.link_up != 0 { "UP" } else { "DOWN" };
    let line = format!("virtio0: flags=<{}>  mtu {}\n", flags, info.mtu);
    let _ = tty::write(line.as_bytes());
    let line = format!(
        "           inet {}  netmask {}  gateway {}\n",
        format_ipv4(info.ipv4),
        format_ipv4(info.subnet_mask),
        format_ipv4(info.gateway)
    );
    let _ = tty::write(line.as_bytes());
    let line = format!("           ether {}\n", format_mac(info.mac));
    let _ = tty::write(line.as_bytes());
    let line = format!("           dns {}\n", format_ipv4(info.dns));
    let _ = tty::write(line.as_bytes());

    core::exit();
}
