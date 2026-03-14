use super::poll::*;

unsafe extern "C" {
    fn close(fd: i32) -> i32;
}

pub fn run_io_tests() -> (u32, u32) {
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

    check!("Pollfd_size_8", core::mem::size_of::<Pollfd>() == 8);
    check!("FdSet_size_128", core::mem::size_of::<FdSet>() == 128);

    check!("POLLIN_eq_1", POLLIN == 1);
    check!("POLLOUT_eq_4", POLLOUT == 4);
    check!("POLLERR_eq_8", POLLERR == 8);
    check!("POLLHUP_eq_16", POLLHUP == 16);
    check!("POLLNVAL_eq_32", POLLNVAL == 32);

    check!("fd_zero_clears_all", unsafe {
        let mut set = FdSet {
            fds_bits: [u64::MAX; 16],
        };
        fd_zero(&mut set);
        set.fds_bits.iter().all(|&x| x == 0)
    });

    check!("fd_set_and_isset", unsafe {
        let mut set = FdSet::default();
        fd_set(5, &mut set);
        fd_isset(5, &set) && !fd_isset(6, &set)
    });

    check!("fd_clr_removes", unsafe {
        let mut set = FdSet::default();
        fd_set(10, &mut set);
        fd_clr(10, &mut set);
        !fd_isset(10, &set)
    });

    check!("fd_set_high_fd", unsafe {
        let mut set = FdSet::default();
        fd_set(1000, &mut set);
        fd_isset(1000, &set)
    });

    check!("fd_isset_out_of_range", unsafe {
        let set = FdSet::default();
        !fd_isset(1024, &set) && !fd_isset(2000, &set)
    });

    check!("fd_set_null_safe", unsafe {
        fd_set(5, core::ptr::null_mut());
        fd_clr(5, core::ptr::null_mut());
        fd_zero(core::ptr::null_mut());
        !fd_isset(5, core::ptr::null())
    });

    check!("pipe_creates_fds", unsafe {
        let mut fds = [0i32; 2];
        let ret = super::misc::pipe(fds.as_mut_ptr());
        if ret == 0 {
            let valid = fds[0] > 0 && fds[1] > 0 && fds[0] != fds[1];
            close(fds[0]);
            close(fds[1]);
            valid
        } else {
            false
        }
    });

    check!("dup_works", unsafe {
        let mut fds = [0i32; 2];
        let ret = super::misc::pipe(fds.as_mut_ptr());
        if ret == 0 {
            let dup_fd = super::misc::dup(fds[0]);
            let valid = dup_fd > 0 && dup_fd != fds[0];
            close(dup_fd);
            close(fds[0]);
            close(fds[1]);
            valid
        } else {
            false
        }
    });

    check!("dup2_works", unsafe {
        let mut fds = [0i32; 2];
        let ret = super::misc::pipe(fds.as_mut_ptr());
        if ret == 0 {
            let target_fd = 50;
            let ret2 = super::misc::dup2(fds[0], target_fd);
            let valid = ret2 == target_fd;
            close(target_fd);
            close(fds[0]);
            close(fds[1]);
            valid
        } else {
            false
        }
    });

    check!("isatty_invalid_fd", unsafe { super::misc::isatty(-1) == 0 });

    check!("access_nonexistent", unsafe {
        super::misc::access(b"/nonexistent_path_xyz\0".as_ptr(), 0) == -1
    });

    check!("umask_returns_0022", super::misc::umask(0) == 0o022);

    check!("chmod_returns_enosys", unsafe {
        super::misc::chmod(b"/tmp\0".as_ptr(), 0o755) == -1
    });

    (pass, fail)
}
