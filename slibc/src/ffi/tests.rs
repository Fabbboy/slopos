use crate::ffi::syscalls::SloposStat;

unsafe extern "C" {
    fn close(fd: i32) -> i32;
}

pub fn run_ffi_syscall_tests() -> (u32, u32) {
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

    check!("SloposStat_size", core::mem::size_of::<SloposStat>() >= 32);

    check!("slopos_yield_no_crash", {
        crate::ffi::syscalls::slopos_yield();
        true
    });

    check!("slopos_yield_no_crash", {
        crate::ffi::syscalls::slopos_yield();
        true
    });

    check!("slopos_clock_gettime_returns_time", {
        let mut sec: i64 = 0;
        let mut nsec: i64 = 0;
        let ret = unsafe { crate::ffi::syscalls::slopos_clock_gettime(1, &mut sec, &mut nsec) };
        ret == 0
    });

    check!("slopos_stat_invalid_path", {
        let path = b"/nonexistent_path_12345\0";
        let mut stat = SloposStat {
            st_mode: 0,
            st_size: 0,
            st_atime: 0,
            st_mtime: 0,
            st_ctime: 0,
        };
        let ret = unsafe { crate::ffi::syscalls::slopos_stat(path.as_ptr(), &mut stat) };
        ret < 0
    });

    check!("slopos_lseek_invalid_fd", {
        let ret = unsafe { crate::ffi::syscalls::slopos_lseek(-1, 0, 0) };
        ret < 0
    });

    check!("slopos_futex_wake_no_waiters", {
        let val: u32 = 0;
        let ret = unsafe { crate::ffi::syscalls::slopos_futex_wake(&val, 1) };
        ret >= 0
    });

    check!("slopos_pipe_creates_fds", {
        let mut fds = [0i32; 2];
        let ret = unsafe { crate::ffi::syscalls::slopos_pipe(fds.as_mut_ptr()) };
        if ret == 0 {
            let valid = fds[0] > 0 && fds[1] > 0 && fds[0] != fds[1];
            unsafe {
                close(fds[0]);
                close(fds[1]);
            }
            valid
        } else {
            false
        }
    });

    (pass, fail)
}
