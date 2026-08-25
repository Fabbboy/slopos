//! Waiting on the live network environment from a boot step.

/// Nudge the NIC's receive path, since nothing else will while a boot step runs.
pub fn pump_rx() {
    if let Some(driver) = crate::net_driver_service::net_driver() {
        (driver.virtnet_force_napi_poll)();
    }
}

/// Polls rather than parks: the caller is a BSP boot step, and a blocking wait
/// would spin without draining RX.
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

pub fn errno_i32(errno: u64) -> i32 {
    errno as i64 as i32
}

pub fn errno_i64(errno: u64) -> i64 {
    errno as i64
}
