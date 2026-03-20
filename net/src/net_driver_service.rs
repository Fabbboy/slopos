use crate::DeviceHandle;

slopos_lib::define_service! {
    net_driver => NetDriverServices {
        @no_wrapper virtio_net_ipv4_addr() -> Option<[u8; 4]>;
        @no_wrapper virtio_net_dns() -> Option<[u8; 4]>;
        @no_wrapper dns_rx_clear();
        @no_wrapper transmit_udp_packet(src_ip: [u8; 4], dst_ip: [u8; 4], src_port: u16, dst_port: u16, payload: &[u8]) -> bool;
        @no_wrapper dns_rx_wait(timeout_ms: u32) -> bool;
        @no_wrapper dns_rx_read(out: &mut [u8]) -> usize;
        @no_wrapper virtio_net_mac() -> Option<[u8; 6]>;
        @no_wrapper get_device_handle() -> Option<&'static DeviceHandle>;
        @no_wrapper dns_intercept_response(payload: &[u8]);
        @no_wrapper virtio_net_is_ready() -> bool;
        @no_wrapper virtio_net_transmit(packet: &[u8]) -> bool;
        @no_wrapper virtnet_force_napi_poll();
    }
}

#[inline]
pub fn virtio_net_ipv4_addr() -> Option<[u8; 4]> {
    if !is_net_driver_initialized() {
        return None;
    }
    (net_driver_services().virtio_net_ipv4_addr)()
}

#[inline]
pub fn virtio_net_dns() -> Option<[u8; 4]> {
    if !is_net_driver_initialized() {
        return None;
    }
    (net_driver_services().virtio_net_dns)()
}

#[inline]
pub fn dns_rx_clear() {
    if !is_net_driver_initialized() {
        return;
    }
    (net_driver_services().dns_rx_clear)()
}

#[inline]
pub fn transmit_udp_packet(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> bool {
    if !is_net_driver_initialized() {
        return false;
    }
    (net_driver_services().transmit_udp_packet)(src_ip, dst_ip, src_port, dst_port, payload)
}

#[inline]
pub fn dns_rx_wait(timeout_ms: u32) -> bool {
    if !is_net_driver_initialized() {
        return false;
    }
    (net_driver_services().dns_rx_wait)(timeout_ms)
}

#[inline]
pub fn dns_rx_read(out: &mut [u8]) -> usize {
    if !is_net_driver_initialized() {
        return 0;
    }
    (net_driver_services().dns_rx_read)(out)
}

#[inline]
pub fn virtio_net_mac() -> Option<[u8; 6]> {
    if !is_net_driver_initialized() {
        return None;
    }
    (net_driver_services().virtio_net_mac)()
}

#[inline]
pub fn get_device_handle() -> Option<&'static DeviceHandle> {
    if !is_net_driver_initialized() {
        return None;
    }
    (net_driver_services().get_device_handle)()
}

#[inline]
pub fn dns_intercept_response(payload: &[u8]) {
    if !is_net_driver_initialized() {
        return;
    }
    (net_driver_services().dns_intercept_response)(payload)
}

#[inline]
pub fn virtio_net_is_ready() -> bool {
    if !is_net_driver_initialized() {
        return false;
    }
    (net_driver_services().virtio_net_is_ready)()
}

#[inline]
pub fn virtio_net_transmit(packet: &[u8]) -> bool {
    if !is_net_driver_initialized() {
        return false;
    }
    (net_driver_services().virtio_net_transmit)(packet)
}

#[inline]
pub fn virtnet_force_napi_poll() {
    if !is_net_driver_initialized() {
        return;
    }
    (net_driver_services().virtnet_force_napi_poll)()
}
