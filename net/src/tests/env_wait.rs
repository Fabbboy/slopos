//! Waiting on the live network environment from a boot step.

/// Nudge the NIC's receive path, since nothing else will while a boot step runs.
pub fn pump_rx() {
    if let Some(driver) = crate::net_driver_service::net_driver() {
        (driver.virtnet_force_napi_poll)();
    }
}

/// Wait up to `failsafe_ms` for `probe` to yield, returning its value and the
/// milliseconds it took.
///
/// A wake would be the better signal and there is none to park on: these run
/// from a BSP boot step, where the condition is reached by the netpoll kthread
/// on another CPU and a blocking wait would spin without draining RX. Elapsed
/// guest time is measured rather than sleeps counted, so an overrunning delay
/// does not multiply into the bound and the loop leaves the moment the
/// condition holds instead of at the granularity of the delay.
pub fn await_env<T>(
    failsafe_ms: u64,
    poll_interval_ms: u32,
    mut probe: impl FnMut() -> Option<T>,
) -> Option<(T, u64)> {
    let start = slopos_kernel_services::clock::uptime_ms();
    loop {
        let elapsed = slopos_kernel_services::clock::uptime_ms().saturating_sub(start);
        if let Some(value) = probe() {
            return Some((value, elapsed));
        }
        if elapsed >= failsafe_ms {
            return None;
        }
        pump_rx();
        slopos_kernel_services::platform::timer_poll_delay_ms(poll_interval_ms);
    }
}

/// `errno` as the negative `i32` a syscall wrapper returns.
pub fn errno_i32(errno: u64) -> i32 {
    errno as i64 as i32
}

/// `errno` as the negative `i64` a syscall returns.
pub fn errno_i64(errno: u64) -> i64 {
    errno as i64
}
