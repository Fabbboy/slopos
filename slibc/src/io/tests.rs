use super::poll::{FdSet, POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT, Pollfd};
use super::shim;

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

    check!("fd_zero_clears_all", {
        let mut set = FdSet {
            fds_bits: [u64::MAX; 16],
        };
        shim::fd_zero(&mut set);
        set.fds_bits.iter().all(|&x| x == 0)
    });

    check!("fd_set_and_isset", {
        let mut set = FdSet::default();
        shim::fd_set(5, &mut set);
        shim::fd_isset(5, &set) && !shim::fd_isset(6, &set)
    });

    check!("fd_clr_removes", {
        let mut set = FdSet::default();
        shim::fd_set(10, &mut set);
        shim::fd_clr(10, &mut set);
        !shim::fd_isset(10, &set)
    });

    check!("fd_set_high_fd", {
        let mut set = FdSet::default();
        shim::fd_set(1000, &mut set);
        shim::fd_isset(1000, &set)
    });

    check!("fd_isset_out_of_range", {
        let set = FdSet::default();
        !shim::fd_isset(1024, &set) && !shim::fd_isset(2000, &set)
    });

    check!("fd_set_null_safe", shim::fd_macros_null_safe());

    check!("pipe_creates_fds", {
        let mut fds = [0i32; 2];
        let ret = shim::pipe(&mut fds);
        if ret == 0 {
            let valid = fds[0] > 0 && fds[1] > 0 && fds[0] != fds[1];
            shim::close(fds[0]);
            shim::close(fds[1]);
            valid
        } else {
            false
        }
    });

    check!("dup_works", {
        let mut fds = [0i32; 2];
        let ret = shim::pipe(&mut fds);
        if ret == 0 {
            let dup_fd = shim::dup(fds[0]);
            let valid = dup_fd > 0 && dup_fd != fds[0];
            shim::close(dup_fd);
            shim::close(fds[0]);
            shim::close(fds[1]);
            valid
        } else {
            false
        }
    });

    check!("dup2_works", {
        let mut fds = [0i32; 2];
        let ret = shim::pipe(&mut fds);
        if ret == 0 {
            let target_fd = 50;
            let ret2 = shim::dup2(fds[0], target_fd);
            let valid = ret2 == target_fd;
            shim::close(target_fd);
            shim::close(fds[0]);
            shim::close(fds[1]);
            valid
        } else {
            false
        }
    });

    check!("isatty_invalid_fd", shim::isatty(-1) == 0);

    check!(
        "access_nonexistent",
        shim::access_cstr(b"/nonexistent_path_xyz\0", 0) == -1
    );

    check!("umask_returns_0022", shim::umask(0) == 0o022);

    check!(
        "chmod_returns_enosys",
        shim::chmod_cstr(b"/tmp\0", 0o755) == -1
    );

    (pass, fail)
}
