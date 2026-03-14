use super::*;

pub fn run_time_tests() -> (u32, u32) {
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

    check!("Timespec_size", core::mem::size_of::<Timespec>() == 16);
    check!("Timeval_size", core::mem::size_of::<Timeval>() == 16);

    check!("CLOCK_REALTIME_eq_0", CLOCK_REALTIME == 0);
    check!("CLOCK_MONOTONIC_eq_1", CLOCK_MONOTONIC == 1);

    check!("clock_gettime_monotonic", unsafe {
        let mut ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let ret = clock_gettime(CLOCK_MONOTONIC, &mut ts);
        ret == 0
    });

    check!("clock_gettime_null_tp", unsafe {
        let ret = clock_gettime(CLOCK_MONOTONIC, core::ptr::null_mut());
        ret == -1
    });

    check!("gettimeofday_null_tv", unsafe {
        let ret = gettimeofday(core::ptr::null_mut(), core::ptr::null_mut());
        ret == 0
    });

    check!("gettimeofday_returns_time", unsafe {
        let mut tv = Timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let ret = gettimeofday(&mut tv, core::ptr::null_mut());
        ret == 0
    });

    check!("time_returns_value", unsafe {
        let t = time(core::ptr::null_mut());
        t >= 0
    });

    check!("time_fills_ptr", unsafe {
        let mut t: i64 = -1;
        let ret = time(&mut t);
        ret >= 0 && t == ret
    });

    check!("nanosleep_zero", unsafe {
        let ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        nanosleep(&ts, core::ptr::null_mut()) == 0
    });

    check!("nanosleep_null_req", unsafe {
        nanosleep(core::ptr::null(), core::ptr::null_mut()) == -1
    });

    check!("usleep_zero", unsafe { usleep(0) == 0 });

    check!("sleep_zero", unsafe { sleep(0) == 0 });

    (pass, fail)
}
