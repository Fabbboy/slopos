use crate::time::Duration;

unsafe extern "C" {
    fn slopos_clock_gettime(clk_id: u64, sec: *mut i64, nsec: *mut i64) -> i32;
}

const CLOCK_REALTIME: u64 = 1;
const CLOCK_MONOTONIC: u64 = 0;

fn clock_gettime(clk_id: u64) -> (i64, i64) {
    let mut sec: i64 = 0;
    let mut nsec: i64 = 0;
    unsafe { slopos_clock_gettime(clk_id, &mut sec, &mut nsec) };
    (sec, nsec)
}

fn timespec_to_duration(sec: i64, nsec: i64) -> Duration {
    if sec < 0 {
        Duration::ZERO
    } else {
        Duration::new(sec as u64, nsec as u32)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Instant(Duration);

impl Instant {
    pub fn now() -> Instant {
        let (sec, nsec) = clock_gettime(CLOCK_MONOTONIC);
        Instant(timespec_to_duration(sec, nsec))
    }

    pub fn checked_sub_instant(&self, other: &Instant) -> Option<Duration> {
        self.0.checked_sub(other.0)
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_add(*other)?))
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_sub(*other)?))
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct SystemTime(Duration);

pub const UNIX_EPOCH: SystemTime = SystemTime(Duration::from_secs(0));

impl SystemTime {
    pub const MAX: SystemTime = SystemTime(Duration::MAX);
    pub const MIN: SystemTime = SystemTime(Duration::ZERO);

    pub fn new(tv_sec: i64, tv_nsec: i32) -> SystemTime {
        SystemTime(Duration::new(tv_sec as u64, tv_nsec as u32))
    }

    pub fn now() -> SystemTime {
        let (sec, nsec) = clock_gettime(CLOCK_REALTIME);
        SystemTime(timespec_to_duration(sec, nsec))
    }

    pub fn sub_time(&self, other: &SystemTime) -> Result<Duration, Duration> {
        self.0.checked_sub(other.0).ok_or_else(|| other.0 - self.0)
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<SystemTime> {
        Some(SystemTime(self.0.checked_add(*other)?))
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<SystemTime> {
        Some(SystemTime(self.0.checked_sub(*other)?))
    }
}
