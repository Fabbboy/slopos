use slopos_abi::net::{UserNetInfo, UserNetMember};

use crate::net_driver_service;

pub fn net_is_ready() -> bool {
    net_driver_service::net_driver()
        .map(|d| (d.is_ready)())
        .unwrap_or(false)
}

pub fn net_get_info(out: &mut UserNetInfo) -> bool {
    match net_driver_service::net_driver() {
        Some(d) => {
            (d.get_info)(out);
            true
        }
        None => false,
    }
}

pub fn net_scan_members(out: *mut UserNetMember, max: usize, active_probe: bool) -> usize {
    net_driver_service::net_driver()
        .map(|d| (d.scan_members)(out, max, active_probe))
        .unwrap_or(0)
}
