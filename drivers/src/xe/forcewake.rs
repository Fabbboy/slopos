use slopos_mm::mmio::MmioRegion;

use crate::pit::pit_poll_delay_ms;

use super::mmio;
use super::regs;

const FORCEWAKE_ACK_TIMEOUT_MS: u32 = 50;

pub fn forcewake_render_on(mmio_region: &MmioRegion) -> bool {
    let val = regs::bit(0);
    let mask = regs::bit(16);
    mmio::write32(mmio_region, regs::FORCEWAKE_RENDER, mask | val);
    wait_for_ack(mmio_region, regs::FORCEWAKE_ACK_RENDER, val)
}

fn wait_for_ack(mmio_region: &MmioRegion, reg: usize, expect: u32) -> bool {
    for _ in 0..FORCEWAKE_ACK_TIMEOUT_MS {
        let ack = mmio::read32(mmio_region, reg);
        if ack == expect {
            return true;
        }
        pit_poll_delay_ms(1);
    }
    false
}
