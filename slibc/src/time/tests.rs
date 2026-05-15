use super::shim;
use super::{CLOCK_MONOTONIC, CLOCK_REALTIME, Timespec, Timeval};

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

    check!("CLOCK_MONOTONIC_eq_0", CLOCK_MONOTONIC == 0);
    check!("CLOCK_REALTIME_eq_1", CLOCK_REALTIME == 1);

    check!("clock_gettime_monotonic", {
        let mut ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        shim::clock_gettime(CLOCK_MONOTONIC, &mut ts) == 0
    });

    check!(
        "clock_gettime_null_tp",
        shim::clock_gettime_null_tp(CLOCK_MONOTONIC) == -1
    );

    check!("gettimeofday_null_tv", shim::gettimeofday_null_tv() == 0);

    check!("gettimeofday_returns_time", {
        let mut tv = Timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        shim::gettimeofday(&mut tv) == 0
    });

    check!("time_returns_value", shim::time(None) >= 0);

    check!("time_fills_ptr", {
        let mut t: i64 = -1;
        let ret = shim::time(Some(&mut t));
        ret >= 0 && t == ret
    });

    check!("nanosleep_zero", {
        let ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        shim::nanosleep(&ts) == 0
    });

    check!("nanosleep_null_req", shim::nanosleep_null_req() == -1);

    check!("usleep_zero", shim::usleep(0) == 0);

    check!("sleep_zero", shim::sleep(0) == 0);

    (pass, fail)
}
