use crate::DeviceHandle;
use slopos_abi::net::{UserNetInfo, UserNetMember};

slopos_service_core::define_service! {
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
        @no_wrapper scan_members(out: *mut UserNetMember, max: usize, active_probe: bool) -> usize;
        @no_wrapper is_ready() -> bool;
        @no_wrapper get_info(out: &mut UserNetInfo);
    }
}

#[inline]
pub fn net_driver() -> Option<&'static NetDriverServices> {
    NET_DRIVER.try_get()
}
