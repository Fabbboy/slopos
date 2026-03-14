use super::addr::*;
use super::dns::*;

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

    check!("inet_addr_127_0_0_1", unsafe {
        let s = b"127.0.0.1\0";
        let addr = inet_addr(s.as_ptr());
        addr != INADDR_NONE && addr == u32::from_ne_bytes([127, 0, 0, 1])
    });
    check!("inet_addr_0_0_0_0", unsafe {
        let s = b"0.0.0.0\0";
        inet_addr(s.as_ptr()) == 0
    });
    check!("inet_addr_255_255_255_255", unsafe {
        let s = b"255.255.255.255\0";
        let addr = inet_addr(s.as_ptr());
        addr == u32::from_ne_bytes([255, 255, 255, 255])
    });
    check!("inet_addr_invalid_empty", unsafe {
        inet_addr(b"\0".as_ptr()) == INADDR_NONE
    });
    check!("inet_addr_invalid_letters", unsafe {
        inet_addr(b"abc\0".as_ptr()) == INADDR_NONE
    });
    check!("inet_addr_null", unsafe {
        inet_addr(core::ptr::null()) == INADDR_NONE
    });
    check!("inet_addr_too_few_octets", unsafe {
        inet_addr(b"1.2.3\0".as_ptr()) == INADDR_NONE
    });
    check!("inet_addr_octet_overflow", unsafe {
        inet_addr(b"1.2.3.256\0".as_ptr()) == INADDR_NONE
    });

    check!("inet_ntoa_127_0_0_1", unsafe {
        let addr = u32::from_ne_bytes([127, 0, 0, 1]);
        let ptr = inet_ntoa(addr);
        !ptr.is_null() && {
            let mut len = 0;
            let mut p = ptr;
            while *p != 0 {
                len += 1;
                p = p.add(1);
            }
            len == 9
        }
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

    check!("getaddrinfo_null_node", unsafe {
        let mut res: *mut AddrInfo = core::ptr::null_mut();
        let ret = getaddrinfo(
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            &mut res,
        );
        ret == EAI_NONAME
    });

    check!("getaddrinfo_numeric_ip", unsafe {
        let mut res: *mut AddrInfo = core::ptr::null_mut();
        let ret = getaddrinfo(
            b"127.0.0.1\0".as_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            &mut res,
        );
        let ok = ret == 0 && !res.is_null() && (*res).ai_family == AF_INET;
        if !res.is_null() {
            freeaddrinfo(res);
        }
        ok
    });

    check!("freeaddrinfo_null_safe", unsafe {
        freeaddrinfo(core::ptr::null_mut());
        true
    });

    (pass, fail)
}
