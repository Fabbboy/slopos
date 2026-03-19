use slopos_lib::IrqMutex;

use crate::DeviceHandle;

#[derive(Clone, Copy)]
pub struct DriverHooks {
    pub virtio_net_ipv4_addr: Option<fn() -> Option<[u8; 4]>>,
    pub virtio_net_dns: Option<fn() -> Option<[u8; 4]>>,
    pub dns_rx_clear: Option<fn()>,
    pub transmit_udp_packet: Option<fn([u8; 4], [u8; 4], u16, u16, &[u8]) -> bool>,
    pub dns_rx_wait: Option<fn(u32) -> bool>,
    pub dns_rx_read: Option<fn(&mut [u8]) -> usize>,
    pub virtio_net_mac: Option<fn() -> Option<[u8; 6]>>,
    pub get_device_handle: Option<fn() -> Option<&'static DeviceHandle>>,
    pub dns_intercept_response: Option<fn(&[u8])>,
    pub virtio_net_is_ready: Option<fn() -> bool>,
    pub virtio_net_transmit: Option<fn(&[u8]) -> bool>,
    pub virtnet_force_napi_poll: Option<fn()>,
}

impl DriverHooks {
    pub const fn empty() -> Self {
        Self {
            virtio_net_ipv4_addr: None,
            virtio_net_dns: None,
            dns_rx_clear: None,
            transmit_udp_packet: None,
            dns_rx_wait: None,
            dns_rx_read: None,
            virtio_net_mac: None,
            get_device_handle: None,
            dns_intercept_response: None,
            virtio_net_is_ready: None,
            virtio_net_transmit: None,
            virtnet_force_napi_poll: None,
        }
    }
}

static DRIVER_HOOKS: IrqMutex<DriverHooks> = IrqMutex::new(DriverHooks::empty());

fn snapshot() -> DriverHooks {
    *DRIVER_HOOKS.lock()
}

pub fn register_driver_hooks(hooks: DriverHooks) {
    *DRIVER_HOOKS.lock() = hooks;
}

pub fn virtio_net_ipv4_addr() -> Option<[u8; 4]> {
    snapshot().virtio_net_ipv4_addr.and_then(|f| f())
}

pub fn virtio_net_dns() -> Option<[u8; 4]> {
    snapshot().virtio_net_dns.and_then(|f| f())
}

pub fn dns_rx_clear() {
    if let Some(f) = snapshot().dns_rx_clear {
        f();
    }
}

pub fn transmit_udp_packet(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> bool {
    snapshot()
        .transmit_udp_packet
        .is_some_and(|f| f(src_ip, dst_ip, src_port, dst_port, payload))
}

pub fn dns_rx_wait(timeout_ms: u32) -> bool {
    snapshot().dns_rx_wait.is_some_and(|f| f(timeout_ms))
}

pub fn dns_rx_read(out: &mut [u8]) -> usize {
    snapshot().dns_rx_read.map_or(0, |f| f(out))
}

pub fn virtio_net_mac() -> Option<[u8; 6]> {
    snapshot().virtio_net_mac.and_then(|f| f())
}

pub fn get_device_handle() -> Option<&'static DeviceHandle> {
    snapshot().get_device_handle.and_then(|f| f())
}

pub fn dns_intercept_response(payload: &[u8]) {
    if let Some(f) = snapshot().dns_intercept_response {
        f(payload);
    }
}

pub fn virtio_net_is_ready() -> bool {
    snapshot().virtio_net_is_ready.is_some_and(|f| f())
}

pub fn virtio_net_transmit(packet: &[u8]) -> bool {
    snapshot().virtio_net_transmit.is_some_and(|f| f(packet))
}

pub fn virtnet_force_napi_poll() {
    if let Some(f) = snapshot().virtnet_force_napi_poll {
        f();
    }
}
