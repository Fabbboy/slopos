//! Core syscalls: yield, exit, sleep, time, CPU info.

use super::numbers::*;
use super::raw::{syscall0, syscall1, syscall2, syscall3};
use slopos_slibc::pal::{Pal, Sys};

#[inline(always)]
pub fn yield_now() {
    let _ = Sys::yield_now();
}

#[inline(always)]
pub fn exit() -> ! {
    Sys::exit(0)
}

#[inline(always)]
pub fn exit_with_code(code: i32) -> ! {
    Sys::exit(code)
}

#[inline(always)]
pub fn sleep_ms(ms: u32) {
    let _ = Sys::sleep_ms(ms as u64);
}

#[inline(always)]
pub fn get_time_ms() -> u64 {
    Sys::get_time_ms()
}

#[inline(always)]
pub fn clock_gettime(ts: &mut Timespec) -> i64 {
    unsafe { syscall2(SYSCALL_CLOCK_GETTIME, CLOCK_MONOTONIC, ts as *mut _ as u64) as i64 }
}

#[inline(always)]
pub fn clock_gettime_ns() -> u64 {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = clock_gettime(&mut ts);
    if rc < 0 {
        return 0;
    }
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

#[inline(always)]
pub fn get_cpu_count() -> u32 {
    unsafe { syscall0(SYSCALL_GET_CPU_COUNT) as u32 }
}

#[inline(always)]
pub fn get_current_cpu() -> u32 {
    unsafe { syscall0(SYSCALL_GET_CURRENT_CPU) as u32 }
}

/// Pin `target` (0 = the calling task) to the CPUs in `affinity`, a bitmask
/// where bit `n` permits CPU `n`. The placement takes effect at the next
/// reschedule; returns 0 on success or a negative errno.
#[inline(always)]
pub fn set_cpu_affinity(target: u32, affinity: u32) -> i64 {
    unsafe { syscall2(SYSCALL_SET_CPU_AFFINITY, target as u64, affinity as u64) as i64 }
}

/// Fills `buf` with cryptographically secure random bytes; returns the count.
#[inline(always)]
pub fn getrandom(buf: &mut [u8]) -> isize {
    unsafe {
        syscall3(
            SYSCALL_GETRANDOM,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            0,
        ) as isize
    }
}

/// Convenience: get a random u32 value.
#[inline(always)]
pub fn random_next() -> u32 {
    let mut buf = [0u8; 4];
    let _ = getrandom(&mut buf);
    u32::from_le_bytes(buf)
}

#[inline(always)]
pub fn sys_info(info: &mut UserSysInfo) -> i64 {
    unsafe { syscall1(SYSCALL_SYS_INFO, info as *mut _ as u64) as i64 }
}

/// Drive the kernel-side userland-test phase from this task. Used by
/// `/sbin/init` when the kernel was booted with `tests=on`. Returns the
/// raw syscall result (0 on success, negative errno otherwise).
#[inline(always)]
pub fn run_userland_tests() -> i64 {
    unsafe { syscall0(SYSCALL_RUN_USERLAND_TESTS) as i64 }
}

#[inline(always)]
pub fn process_list(buf: &mut [UserTaskEntry]) -> i64 {
    unsafe {
        syscall2(
            SYSCALL_PROCESS_LIST,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        ) as i64
    }
}

#[inline(always)]
pub fn cpu_info(info: &mut UserCpuInfo) -> i64 {
    unsafe { syscall1(SYSCALL_CPU_INFO, info as *mut _ as u64) as i64 }
}

#[inline(always)]
pub fn percpu_stats(buf: &mut [UserPerCpuStats]) -> i64 {
    unsafe {
        syscall2(
            SYSCALL_PERCPU_STATS,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        ) as i64
    }
}
