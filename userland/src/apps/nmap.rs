use crate::syscall::{
    USER_NET_MAX_MEMBERS, UserNetInfo, UserNetMember, core,
    net::{net_info, net_scan},
    tty,
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
    let msg = format!(
        "host {}  mac {}\n",
        format_ipv4(member.ipv4),
        format_mac(member.mac)
    );
    let _ = tty::write(msg.as_bytes());
}

pub fn nmap_main() -> ! {
    let mut info = UserNetInfo::default();
    if net_info(&mut info) != 0 {
        let _ = tty::write(b"nmap: net_info syscall failed\n");
        core::exit_with_code(1);
    }

    if info.nic_ready == 0 {
        let _ = tty::write(b"nmap: no network interface detected\n");
        core::exit_with_code(1);
    }

    if info.link_up == 0 {
        let _ = tty::write(b"nmap: network link is down\n");
        core::exit_with_code(1);
    }

    if info.ipv4 == [0; 4] {
        let _ = tty::write(b"nmap: no IP address (DHCP failed?)\n");
        core::exit_with_code(1);
    }

    let msg = format!("nmap: interface virtio0 ip {}\n", format_ipv4(info.ipv4));
    let _ = tty::write(msg.as_bytes());
    let _ = tty::write(b"nmap: scanning...\n");

    let mut members = [UserNetMember::default(); USER_NET_MAX_MEMBERS];
    let count = net_scan(&mut members, true);

    if count < 0 {
        let _ = tty::write(b"nmap: scan syscall failed\n");
        core::exit_with_code(1);
    }

    if count == 0 {
        let _ = tty::write(b"nmap: no hosts discovered on network\n");
        core::exit_with_code(1);
    }

    let _ = tty::write(b"nmap: discovered members\n");
    let mut idx = 0usize;
    while idx < count as usize && idx < members.len() {
        print_member(&members[idx]);
        idx += 1;
    }

    core::exit();
}
