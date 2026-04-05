use crate::errno::Errno;
use crate::pal::Pal;
use crate::pal::raw::*;
use slopos_abi::syscall::*;

pub struct Sys;

#[inline]
fn to_result(ret: u64) -> Result<u64, Errno> {
    crate::demux(ret).map_err(|e| {
        let errno = Errno::from(e);
        crate::errno::errno_set(errno.raw());
        errno
    })
}

impl Pal for Sys {
    fn open(path: *const u8, flags: i32, mode: u32) -> Result<i32, Errno> {
        let _ = mode;
        let ret = unsafe { syscall2(SYSCALL_FS_OPEN, path as u64, flags as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn close(fd: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_FS_CLOSE, fd as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn read(fd: i32, buf: *mut u8, count: usize) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_FS_READ, fd as u64, buf as u64, count as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn write(fd: i32, buf: *const u8, count: usize) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_FS_WRITE, fd as u64, buf as u64, count as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn lseek(fd: i32, offset: i64, whence: i32) -> Result<i64, Errno> {
        let ret = unsafe { syscall3(SYSCALL_LSEEK, fd as u64, offset as u64, whence as u64) };
        let val = to_result(ret)?;
        Ok(val as i64)
    }

    fn fstat(fd: i32, stat_buf: *mut u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_FSTAT, fd as u64, stat_buf as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn stat(path: *const u8, stat_buf: *mut u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_FS_STAT, path as u64, stat_buf as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn mkdir(path: *const u8, mode: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_FS_MKDIR, path as u64, mode as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn unlink(path: *const u8) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_FS_UNLINK, path as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn rename(old: *const u8, new: *const u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_RENAME, old as u64, new as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn dup(fd: i32) -> Result<i32, Errno> {
        let ret = unsafe { syscall1(SYSCALL_DUP, fd as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn dup2(old: i32, new: i32) -> Result<i32, Errno> {
        let ret = unsafe { syscall2(SYSCALL_DUP2, old as u64, new as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn fcntl(fd: i32, cmd: i32, arg: u64) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_FCNTL, fd as u64, cmd as u64, arg) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn pipe(fds: *mut [i32; 2]) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_PIPE, fds as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn poll(fds: *mut u8, nfds: u32, timeout: i32) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_POLL, fds as u64, nfds as u64, timeout as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn select(
        nfds: i32,
        readfds: *mut u8,
        writefds: *mut u8,
        exceptfds: *mut u8,
        timeout: *mut u8,
    ) -> Result<i32, Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_SELECT,
                nfds as u64,
                readfds as u64,
                writefds as u64,
                exceptfds as u64,
                timeout as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn ioctl(fd: i32, request: u64, arg: u64) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_IOCTL, fd as u64, request, arg) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn list(path: *const u8, buf: *mut u8, buf_len: usize) -> Result<usize, Errno> {
        use slopos_abi::{USER_FS_MAX_ENTRIES, UserFsEntry, UserFsList};

        let mut entries = [UserFsEntry::new(); USER_FS_MAX_ENTRIES as usize];
        let mut hdr = UserFsList {
            entries: entries.as_mut_ptr(),
            max_entries: USER_FS_MAX_ENTRIES,
            count: 0,
        };

        let ret = unsafe {
            syscall2(
                SYSCALL_FS_LIST,
                path as u64,
                &mut hdr as *mut UserFsList as u64,
            )
        };
        to_result(ret)?;

        let count = hdr.count as usize;
        let mut pos = 0usize;
        for i in 0..count {
            let entry = &entries[i];
            let name_len = entry
                .name
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(entry.name.len());
            if name_len == 0 {
                continue;
            }
            let needed = if pos == 0 { name_len } else { name_len + 1 };
            if pos + needed > buf_len {
                return Err(Errno::from(crate::error::SyscallError::from_errno(34)));
            }
            if pos > 0 {
                unsafe { *buf.add(pos) = b'\n' };
                pos += 1;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(entry.name.as_ptr(), buf.add(pos), name_len);
            }
            pos += name_len;
        }

        Ok(pos)
    }

    fn brk(addr: *mut u8) -> Result<*mut u8, Errno> {
        let ret = unsafe { syscall1(SYSCALL_BRK, addr as u64) };
        let val = to_result(ret)?;
        Ok(val as *mut u8)
    }

    fn mmap(
        addr: *mut u8,
        len: usize,
        prot: u64,
        flags: u64,
        fd: i32,
        offset: u64,
    ) -> Result<*mut u8, Errno> {
        let ret = unsafe {
            syscall6(
                SYSCALL_MMAP,
                addr as u64,
                len as u64,
                prot,
                flags,
                fd as u64,
                offset,
            )
        };
        let val = to_result(ret)?;
        Ok(val as *mut u8)
    }

    fn munmap(addr: *mut u8, len: usize) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_MUNMAP, addr as u64, len as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn mprotect(addr: *mut u8, len: usize, prot: u64) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_MPROTECT, addr as u64, len as u64, prot) };
        to_result(ret)?;
        Ok(())
    }

    fn fork() -> Result<i32, Errno> {
        let ret = unsafe { syscall0(SYSCALL_FORK) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn exec(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_EXEC, path as u64, argv as u64, envp as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn waitpid(pid: i32, status: *mut i32, options: i32) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_WAITPID, pid as u64, status as u64, options as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn exit(code: i32) -> ! {
        unsafe {
            syscall1(SYSCALL_EXIT, code as u64);
        }
        loop {
            core::hint::spin_loop();
        }
    }

    fn getpid() -> i32 {
        unsafe { syscall0(SYSCALL_GETPID) as i32 }
    }

    fn getppid() -> i32 {
        unsafe { syscall0(SYSCALL_GETPPID) as i32 }
    }

    fn getuid() -> u32 {
        unsafe { syscall0(SYSCALL_GETUID) as u32 }
    }

    fn getgid() -> u32 {
        unsafe { syscall0(SYSCALL_GETGID) as u32 }
    }

    fn geteuid() -> u32 {
        unsafe { syscall0(SYSCALL_GETEUID) as u32 }
    }

    fn getegid() -> u32 {
        unsafe { syscall0(SYSCALL_GETEGID) as u32 }
    }

    fn setpgid(pid: i32, pgid: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_SETPGID, pid as u64, pgid as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn getpgid(pid: i32) -> Result<i32, Errno> {
        let ret = unsafe { syscall1(SYSCALL_GETPGID, pid as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn setsid() -> Result<i32, Errno> {
        let ret = unsafe { syscall0(SYSCALL_SETSID) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn chdir(path: *const u8) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_CHDIR, path as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn getcwd(buf: *mut u8, size: usize) -> Result<usize, Errno> {
        let ret = unsafe { syscall2(SYSCALL_GETCWD, buf as u64, size as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn clone(
        flags: u64,
        stack: *mut u8,
        parent_tid: *mut i32,
        child_tid: *mut i32,
        tls: u64,
    ) -> Result<i32, Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_CLONE,
                flags,
                stack as u64,
                parent_tid as u64,
                child_tid as u64,
                tls,
            )
        };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn futex_wait(addr: *const u32, val: u32, timeout_ms: u64) -> Result<(), Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_FUTEX,
                addr as u64,
                FUTEX_WAIT,
                val as u64,
                timeout_ms,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn futex_wake(addr: *const u32, count: u32) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_FUTEX, addr as u64, FUTEX_WAKE, count as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn arch_prctl_set_fs(base: u64) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_ARCH_PRCTL, ARCH_SET_FS, base) };
        to_result(ret)?;
        Ok(())
    }

    fn arch_prctl_get_fs() -> Result<u64, Errno> {
        let mut base = 0u64;
        let ret = unsafe {
            syscall2(
                SYSCALL_ARCH_PRCTL,
                ARCH_GET_FS,
                (&mut base as *mut u64) as u64,
            )
        };
        to_result(ret)?;
        Ok(base)
    }

    fn rt_sigaction(
        sig: i32,
        act: *const u8,
        oldact: *mut u8,
        sigsetsize: usize,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_RT_SIGACTION,
                sig as u64,
                act as u64,
                oldact as u64,
                sigsetsize as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn rt_sigprocmask(
        how: i32,
        set: *const u64,
        oldset: *mut u64,
        sigsetsize: usize,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_RT_SIGPROCMASK,
                how as u64,
                set as u64,
                oldset as u64,
                sigsetsize as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn kill(pid: i32, sig: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_KILL, pid as u64, sig as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn rt_sigreturn() -> ! {
        unsafe {
            syscall0(SYSCALL_RT_SIGRETURN);
        }
        loop {
            core::hint::spin_loop();
        }
    }

    fn socket(domain: i32, sock_type: i32, protocol: i32) -> Result<i32, Errno> {
        let ret = unsafe {
            syscall3(
                SYSCALL_SOCKET,
                domain as u64,
                sock_type as u64,
                protocol as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn bind(fd: i32, addr: *const u8, addrlen: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_BIND, fd as u64, addr as u64, addrlen as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn listen(fd: i32, backlog: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_LISTEN, fd as u64, backlog as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn accept(fd: i32, addr: *mut u8, addrlen: *mut u32) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_ACCEPT, fd as u64, addr as u64, addrlen as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn connect(fd: i32, addr: *const u8, addrlen: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_CONNECT, fd as u64, addr as u64, addrlen as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn send(fd: i32, buf: *const u8, len: usize, flags: i32) -> Result<usize, Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_SEND,
                fd as u64,
                buf as u64,
                len as u64,
                flags as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> Result<usize, Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_RECV,
                fd as u64,
                buf as u64,
                len as u64,
                flags as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn sendto(
        fd: i32,
        buf: *const u8,
        len: usize,
        flags: i32,
        addr: *const u8,
        addrlen: u32,
    ) -> Result<usize, Errno> {
        let ret = unsafe {
            syscall6(
                SYSCALL_SENDTO,
                fd as u64,
                buf as u64,
                len as u64,
                flags as u64,
                addr as u64,
                addrlen as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn recvfrom(
        fd: i32,
        buf: *mut u8,
        len: usize,
        flags: i32,
        addr: *mut u8,
        addrlen: *mut u32,
    ) -> Result<usize, Errno> {
        let ret = unsafe {
            syscall6(
                SYSCALL_RECVFROM,
                fd as u64,
                buf as u64,
                len as u64,
                flags as u64,
                addr as u64,
                addrlen as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn setsockopt(
        fd: i32,
        level: i32,
        optname: i32,
        optval: *const u8,
        optlen: u32,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_SETSOCKOPT,
                fd as u64,
                level as u64,
                optname as u64,
                optval as u64,
                optlen as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn getsockopt(
        fd: i32,
        level: i32,
        optname: i32,
        optval: *mut u8,
        optlen: *mut u32,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_GETSOCKOPT,
                fd as u64,
                level as u64,
                optname as u64,
                optval as u64,
                optlen as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn shutdown(fd: i32, how: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_SHUTDOWN, fd as u64, how as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn getpeername(fd: i32, addr: *mut u8, addrlen: *mut u32) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_GETPEERNAME, fd as u64, addr as u64, addrlen as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn getsockname(fd: i32, addr: *mut u8, addrlen: *mut u32) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_GETSOCKNAME, fd as u64, addr as u64, addrlen as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn resolve(hostname: *const u8, hostname_len: usize, result: *mut u8) -> Result<(), Errno> {
        let ret = unsafe {
            syscall3(
                SYSCALL_RESOLVE,
                hostname as u64,
                hostname_len as u64,
                result as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn clock_gettime(clk_id: u64, tp: *mut u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_CLOCK_GETTIME, clk_id, tp as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn get_time_ms() -> u64 {
        unsafe { syscall0(SYSCALL_GET_TIME_MS) }
    }

    fn sleep_ms(ms: u64) {
        unsafe {
            syscall1(SYSCALL_SLEEP_MS, ms);
        }
    }

    fn yield_now() {
        unsafe {
            syscall0(SYSCALL_YIELD);
        }
    }

    fn halt() -> ! {
        unsafe {
            syscall0(SYSCALL_HALT);
        }
        loop {
            core::hint::spin_loop();
        }
    }

    fn reboot() -> ! {
        unsafe {
            syscall0(SYSCALL_REBOOT);
        }
        loop {
            core::hint::spin_loop();
        }
    }

    fn sendmsg(fd: i32, msg: *const MsgHdr, flags: i32) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_SENDMSG, fd as u64, msg as u64, flags as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn recvmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_RECVMSG, fd as u64, msg as u64, flags as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn memfd_create(flags: u32) -> Result<i32, Errno> {
        let ret = unsafe { syscall1(SYSCALL_MEMFD_CREATE, flags as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn ftruncate(fd: i32, size: u64) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_FTRUNCATE, fd as u64, size) };
        to_result(ret)?;
        Ok(())
    }
}
