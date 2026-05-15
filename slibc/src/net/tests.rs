use super::addr::*;
use super::dns::*;
use super::shim;

pub fn run_net_tests() -> (u32, u32) {
    let mut pass = 0u32;
    let mut fail = 0u32;

    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                pass += 1;
            } else {
                fail += 1;
            }
        };
    }

    check!("SockAddr_size_16", core::mem::size_of::<SockAddr>() == 16);
    check!(
        "SockAddrIn_size_16",
        core::mem::size_of::<SockAddrIn>() == 16
    );
    check!("AddrInfo_has_fields", core::mem::size_of::<AddrInfo>() > 0);

    check!("AF_INET_eq_2", AF_INET == 2);
    check!("AF_UNIX_eq_1", AF_UNIX == 1);
    check!("AF_INET6_eq_10", AF_INET6 == 10);
    check!("SOCK_STREAM_eq_1", SOCK_STREAM == 1);
    check!("SOCK_DGRAM_eq_2", SOCK_DGRAM == 2);
    check!("IPPROTO_TCP_eq_6", IPPROTO_TCP == 6);
    check!("IPPROTO_UDP_eq_17", IPPROTO_UDP == 17);
    check!("SHUT_RD_eq_0", SHUT_RD == 0);
    check!("SHUT_WR_eq_1", SHUT_WR == 1);
    check!("SHUT_RDWR_eq_2", SHUT_RDWR == 2);

    check!("htons_roundtrip", ntohs(htons(0x1234)) == 0x1234);
    check!("htonl_roundtrip", ntohl(htonl(0xDEADBEEF)) == 0xDEADBEEF);
    check!("htons_80", htons(80) == 80u16.to_be());
    check!("htonl_zero", htonl(0) == 0);
    check!("ntohs_identity_be", {
        let val: u16 = 0x0050;
        ntohs(val) == u16::from_be(val)
    });

    check!("inet_addr_127_0_0_1", {
        let addr = shim::inet_addr_cstr(b"127.0.0.1\0");
        addr != INADDR_NONE && addr == u32::from_ne_bytes([127, 0, 0, 1])
    });
    check!("inet_addr_0_0_0_0", shim::inet_addr_cstr(b"0.0.0.0\0") == 0);
    check!("inet_addr_255_255_255_255", {
        let addr = shim::inet_addr_cstr(b"255.255.255.255\0");
        addr == u32::from_ne_bytes([255, 255, 255, 255])
    });
    check!("inet_addr_invalid_empty", shim::inet_addr_is_none(b"\0"));
    check!(
        "inet_addr_invalid_letters",
        shim::inet_addr_invalid_letters() == INADDR_NONE
    );
    check!("inet_addr_null", shim::inet_addr_null() == INADDR_NONE);
    check!(
        "inet_addr_too_few_octets",
        shim::inet_addr_is_none(b"1.2.3\0")
    );
    check!(
        "inet_addr_octet_overflow",
        shim::inet_addr_is_none(b"1.2.3.256\0")
    );

    check!("inet_ntoa_127_0_0_1", {
        let addr = u32::from_ne_bytes([127, 0, 0, 1]);
        shim::inet_ntoa_len(addr) == Some(9)
    });

    check!("EAI_NONAME_negative", EAI_NONAME < 0);
    check!("gai_strerror_noname", {
        let p = gai_strerror(EAI_NONAME);
        !p.is_null()
    });
    check!("gai_strerror_success", {
        let p = gai_strerror(0);
        !p.is_null()
    });

    check!(
        "getaddrinfo_null_node",
        shim::getaddrinfo_all_null() == EAI_NONAME
    );

    check!("getaddrinfo_numeric_ip", {
        let (ret, family) = shim::getaddrinfo_numeric(b"127.0.0.1\0");
        ret == 0 && family == Some(AF_INET)
    });

    check!("freeaddrinfo_null_safe", {
        shim::freeaddrinfo_null_safe();
        true
    });

    (pass, fail)
}
