use crate::syscall::{
    USER_NET_MAX_MEMBERS, UserNetInfo, UserNetMember,
    net::{net_info, net_scan},
};

fn format_ipv4(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

fn print_member(member: &UserNetMember) {
    println!(
        "host {}  mac {}",
        format_ipv4(member.ipv4),
        format_mac(member.mac)
    );
}

pub fn nmap_main() {
    let mut info = UserNetInfo::default();
    if net_info(&mut info) != 0 {
        eprintln!("nmap: net_info syscall failed");
        std::process::exit(1);
    }

    if info.nic_ready == 0 {
        eprintln!("nmap: no network interface detected");
        std::process::exit(1);
    }

    if info.link_up == 0 {
        eprintln!("nmap: network link is down");
        std::process::exit(1);
    }

    if info.ipv4 == [0; 4] {
        eprintln!("nmap: no IP address (DHCP failed?)");
        std::process::exit(1);
    }

    println!("nmap: interface virtio0 ip {}", format_ipv4(info.ipv4));
    println!("nmap: scanning...");

    let mut members = [UserNetMember::default(); USER_NET_MAX_MEMBERS];
    let count = net_scan(&mut members, true);

    if count < 0 {
        eprintln!("nmap: scan syscall failed");
        std::process::exit(1);
    }

    if count == 0 {
        eprintln!("nmap: no hosts discovered on network");
        std::process::exit(1);
    }

    println!("nmap: discovered members");
    let mut idx = 0usize;
    while idx < count as usize && idx < members.len() {
        print_member(&members[idx]);
        idx += 1;
    }

    std::process::exit(0);
}
