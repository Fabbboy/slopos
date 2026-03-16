crate::define_service! {
    socket => SocketServices {
        create(domain: u16, sock_type: u16, protocol: u16) -> i32;
        bind(sock_idx: u32, addr: [u8; 4], port: u16) -> i32;
        listen(sock_idx: u32, backlog: u32) -> i32;
        accept(sock_idx: u32, peer_addr: *mut [u8; 4], peer_port: *mut u16) -> i32;
        connect(sock_idx: u32, addr: [u8; 4], port: u16) -> i32;
        send(sock_idx: u32, data: *const u8, len: usize) -> i64;
        recv(sock_idx: u32, buf: *mut u8, len: usize) -> i64;
        sendto(sock_idx: u32, data: *const u8, len: usize, dst_ip: [u8; 4], dst_port: u16) -> i64;
        recvfrom(
            sock_idx: u32,
            buf: *mut u8,
            len: usize,
            src_ip: *mut [u8; 4],
            src_port: *mut u16,
        ) -> i64;
        close(sock_idx: u32) -> i32;
        poll_readable(sock_idx: u32) -> u32;
        poll_writable(sock_idx: u32) -> u32;
        poll_enqueue_recv(sock_idx: u32) -> bool;
        poll_dequeue_recv(sock_idx: u32);
        set_nonblocking(sock_idx: u32, nonblocking: bool) -> i32;
        setsockopt(sock_idx: u32, level: i32, optname: i32, val: *const u8, len: usize) -> i32;
        getsockopt(sock_idx: u32, level: i32, optname: i32, out: *mut u8, len: usize) -> i32;
        shutdown(sock_idx: u32, how: i32) -> i32;
    }
}
