use crate::syscall::{UserNetInfo, net::net_info};

fn format_ipv4(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

pub fn ifconfig_main() {
    let mut info = UserNetInfo::default();
    if net_info(&mut info) != 0 {
        eprintln!("ifconfig: net_info syscall failed");
        std::process::exit(1);
    }

    if info.nic_ready == 0 {
        eprintln!("ifconfig: no network interface");
        std::process::exit(1);
    }

    let flags = if info.link_up != 0 { "UP" } else { "DOWN" };
    println!("virtio0: flags=<{}>  mtu {}", flags, info.mtu);
    println!(
        "           inet {}  netmask {}  gateway {}",
        format_ipv4(info.ipv4),
        format_ipv4(info.subnet_mask),
        format_ipv4(info.gateway)
    );
    println!("           ether {}", format_mac(info.mac));
    println!("           dns {}", format_ipv4(info.dns));

    std::process::exit(0);
}
