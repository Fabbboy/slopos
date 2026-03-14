pub mod raw;
pub mod slopos;
pub mod syscall;

pub use slopos::Sys;

use crate::errno::Errno;

pub trait Pal {
    fn open(path: *const u8, flags: i32, mode: u32) -> Result<i32, Errno>;
    fn close(fd: i32) -> Result<(), Errno>;
    fn read(fd: i32, buf: *mut u8, count: usize) -> Result<usize, Errno>;
    fn write(fd: i32, buf: *const u8, count: usize) -> Result<usize, Errno>;
    fn lseek(fd: i32, offset: i64, whence: i32) -> Result<i64, Errno>;
    fn fstat(fd: i32, stat_buf: *mut u8) -> Result<(), Errno>;
    fn stat(path: *const u8, stat_buf: *mut u8) -> Result<(), Errno>;
    fn mkdir(path: *const u8, mode: u32) -> Result<(), Errno>;
    fn unlink(path: *const u8) -> Result<(), Errno>;
    fn rename(old: *const u8, new: *const u8) -> Result<(), Errno>;
    fn dup(fd: i32) -> Result<i32, Errno>;
    fn dup2(old: i32, new: i32) -> Result<i32, Errno>;
    fn fcntl(fd: i32, cmd: i32, arg: u64) -> Result<i32, Errno>;
    fn pipe(fds: *mut [i32; 2]) -> Result<(), Errno>;
    fn poll(fds: *mut u8, nfds: u32, timeout: i32) -> Result<i32, Errno>;
    fn select(
        nfds: i32,
        readfds: *mut u8,
        writefds: *mut u8,
        exceptfds: *mut u8,
        timeout: *mut u8,
    ) -> Result<i32, Errno>;
    fn ioctl(fd: i32, request: u64, arg: u64) -> Result<i32, Errno>;
    fn list(path: *const u8, buf: *mut u8, buf_len: usize) -> Result<usize, Errno>;

    fn brk(addr: *mut u8) -> Result<*mut u8, Errno>;
    fn mmap(
        addr: *mut u8,
        len: usize,
        prot: u64,
        flags: u64,
        fd: i32,
        offset: u64,
    ) -> Result<*mut u8, Errno>;
    fn munmap(addr: *mut u8, len: usize) -> Result<(), Errno>;
    fn mprotect(addr: *mut u8, len: usize, prot: u64) -> Result<(), Errno>;

    fn fork() -> Result<i32, Errno>;
    fn exec(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> Result<(), Errno>;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> Result<i32, Errno>;
    fn exit(code: i32) -> !;
    fn getpid() -> i32;
    fn getppid() -> i32;
    fn getuid() -> u32;
    fn getgid() -> u32;
    fn geteuid() -> u32;
    fn getegid() -> u32;
    fn setpgid(pid: i32, pgid: i32) -> Result<(), Errno>;
    fn getpgid(pid: i32) -> Result<i32, Errno>;
    fn setsid() -> Result<i32, Errno>;
    fn chdir(path: *const u8) -> Result<(), Errno>;
    fn getcwd(buf: *mut u8, size: usize) -> Result<usize, Errno>;

    fn clone(
        flags: u64,
        stack: *mut u8,
        parent_tid: *mut i32,
        child_tid: *mut i32,
        tls: u64,
    ) -> Result<i32, Errno>;
    fn futex_wait(addr: *const u32, val: u32, timeout_ms: u64) -> Result<(), Errno>;
    fn futex_wake(addr: *const u32, count: u32) -> Result<i32, Errno>;
    fn arch_prctl_set_fs(base: u64) -> Result<(), Errno>;
    fn arch_prctl_get_fs() -> Result<u64, Errno>;

    fn rt_sigaction(
        sig: i32,
        act: *const u8,
        oldact: *mut u8,
        sigsetsize: usize,
    ) -> Result<(), Errno>;
    fn rt_sigprocmask(
        how: i32,
        set: *const u64,
        oldset: *mut u64,
        sigsetsize: usize,
    ) -> Result<(), Errno>;
    fn kill(pid: i32, sig: i32) -> Result<(), Errno>;
    fn rt_sigreturn() -> !;

    fn socket(domain: i32, sock_type: i32, protocol: i32) -> Result<i32, Errno>;
    fn bind(fd: i32, addr: *const u8, addrlen: u32) -> Result<(), Errno>;
    fn listen(fd: i32, backlog: i32) -> Result<(), Errno>;
    fn accept(fd: i32, addr: *mut u8, addrlen: *mut u32) -> Result<i32, Errno>;
    fn connect(fd: i32, addr: *const u8, addrlen: u32) -> Result<(), Errno>;
    fn send(fd: i32, buf: *const u8, len: usize, flags: i32) -> Result<usize, Errno>;
    fn recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> Result<usize, Errno>;
    fn sendto(
        fd: i32,
        buf: *const u8,
        len: usize,
        flags: i32,
        addr: *const u8,
        addrlen: u32,
    ) -> Result<usize, Errno>;
    fn recvfrom(
        fd: i32,
        buf: *mut u8,
        len: usize,
        flags: i32,
        addr: *mut u8,
        addrlen: *mut u32,
    ) -> Result<usize, Errno>;
    fn setsockopt(
        fd: i32,
        level: i32,
        optname: i32,
        optval: *const u8,
        optlen: u32,
    ) -> Result<(), Errno>;
    fn getsockopt(
        fd: i32,
        level: i32,
        optname: i32,
        optval: *mut u8,
        optlen: *mut u32,
    ) -> Result<(), Errno>;
    fn shutdown(fd: i32, how: i32) -> Result<(), Errno>;
    fn resolve(hostname: *const u8, hostname_len: usize, result: *mut u8) -> Result<(), Errno>;

    fn clock_gettime(clk_id: u64, tp: *mut u8) -> Result<(), Errno>;
    fn get_time_ms() -> u64;
    fn sleep_ms(ms: u64);

    fn yield_now();
    fn halt() -> !;
    fn reboot() -> !;
}
