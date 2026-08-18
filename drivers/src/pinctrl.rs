//! Minimal Intel PCH GPIO/pinctrl interrupt path: enough of the community/pad
//! register model to service the I²C-HID touchpad's GpioInt on Tiger Lake-LP
//! (ACPI `INTC1055`, shared with Alder Lake-P), whose pad sits in community 1.
//!
//! The community windows sit at `SBREG_BAR + community_offset`, neither value
//! discoverable at runtime: the P2SB device exposing `SBREG_BAR` is
//! firmware-hidden, and the `_CRS` computing the windows reads an
//! `OperationRegion` the AML reader cannot resolve. Both are SoC constants
//! confirmed against silicon by [`init_for_pad`]; a wrong one fails validation
//! and the caller falls back to polling.

use slopos_abi::addr::PhysAddr;
use slopos_mm::mmio::{MmioRegion, MmioRegionExt};
use slopos_ostd::sync::OnceLock;

/// Community base + 0x0c: offset from that base to the pad-config register block.
const PADBAR: usize = 0x00c;
/// PADCFG0..3 = 4 dwords = 16 bytes per pad.
const PAD_NREGS: usize = 4;

/// Interrupt status / enable bases for the Tiger Lake-LP register variant; one
/// 32-bit register per pad group.
const TGL_GPI_IS: usize = 0x100;
const TGL_GPI_IE: usize = 0x120;

const PADCFG0_RXEVCFG_MASK: u32 = 3 << 25;
const PADCFG0_RXEVCFG_EDGE: u32 = 1 << 25; // (level = 0)
const PADCFG0_RXINV: u32 = 1 << 23;
const PADCFG0_GPIROUTIOXAPIC: u32 = 1 << 20;
const PADCFG0_GPIORXDIS: u32 = 1 << 9;

// Architectural TGL-LP / ADL-P PCH constants: communities (0,0,66)(1,67,170)
// (2,171,259)(3,260,276) at `SBREG_BAR + {0x6e0000,0x6d0000,0x6a0000,0x690000}`.
// Only community 1 — the touchpad pad `ISH_GP_4` (pin 116) — is modelled, and
// `gpio_base` is the gpiochip line for `pin_lo`, the numbering ACPI GpioInt uses.

const SBREG_BAR: u32 = 0xfd00_0000;
const COMMUNITY1_OFFSET: u32 = 0x006d_0000;
const COMMUNITY_LEN: usize = 0x1_0000;
const COMMUNITY1_PIN_LO: u16 = 67;

struct Gpp {
    reg_num: u8,
    pin_lo: u16,
    pin_hi: u16,
    gpio_base: u16,
}

static COMMUNITY1_GPPS: &[Gpp] = &[
    Gpp {
        reg_num: 0,
        pin_lo: 67,
        pin_hi: 74,
        gpio_base: 96,
    },
    Gpp {
        reg_num: 1,
        pin_lo: 75,
        pin_hi: 98,
        gpio_base: 128,
    },
    Gpp {
        reg_num: 2,
        pin_lo: 99,
        pin_hi: 119,
        gpio_base: 160,
    },
    Gpp {
        reg_num: 3,
        pin_lo: 120,
        pin_hi: 143,
        gpio_base: 192,
    },
    Gpp {
        reg_num: 4,
        pin_lo: 144,
        pin_hi: 170,
        gpio_base: 224,
    },
];

/// Pad index for PADCFG addressing, plus the `GPI_IS`/`GPI_IE` register and bit.
struct PadLoc {
    padno: usize,
    reg_num: u8,
    gpp_offset: u32,
}

/// ACPI GpioInt pin (a gpiochip line) to pinctrl pin index; `None` outside
/// community 1. Line 177 → pin 116.
fn pin_for_crs_gpio(line: u16) -> Option<u16> {
    for g in COMMUNITY1_GPPS {
        let span = g.pin_hi - g.pin_lo;
        if line >= g.gpio_base && line <= g.gpio_base + span {
            return Some(g.pin_lo + (line - g.gpio_base));
        }
    }
    None
}

fn resolve_pad(pin: u16) -> Option<PadLoc> {
    for g in COMMUNITY1_GPPS {
        if pin >= g.pin_lo && pin <= g.pin_hi {
            return Some(PadLoc {
                padno: (pin - COMMUNITY1_PIN_LO) as usize,
                reg_num: g.reg_num,
                gpp_offset: (pin - g.pin_lo) as u32,
            });
        }
    }
    None
}

struct Pinctrl {
    mmio: MmioRegion,
    padcfg0_off: usize,
    is_off: usize,
    ie_off: usize,
    bit: u32,
}

static PINCTRL: OnceLock<Pinctrl> = OnceLock::new();

/// Program the touchpad's GpioInt pad as an IO-APIC-routed interrupt, left
/// **masked** until [`pad_irq_unmask`] once the cascade handler is registered.
/// `crs_gpio_line` is the pin from the device's `_CRS`. Returns `None` — caller
/// polls instead — when the pad is outside the supported community or the mapped
/// window fails validation.
pub fn init_for_pad(crs_gpio_line: u16, edge: bool, active_low: bool) -> Option<u16> {
    let pin = pin_for_crs_gpio(crs_gpio_line)?;
    let pad = resolve_pad(pin)?;

    let base = SBREG_BAR + COMMUNITY1_OFFSET;
    let mmio = MmioRegion::map(PhysAddr::new(base as u64), COMMUNITY_LEN)?;

    // A real GPIO community has a small in-window PADBAR and a configured,
    // non-floating PADCFG0; a wrong base reads back 0xffffffff or garbage.
    let padbar = mmio.read::<u32>(PADBAR) as usize;
    if padbar < 0x10 || padbar >= COMMUNITY_LEN {
        return None;
    }
    let padcfg0_off = padbar + pad.padno * PAD_NREGS * 4;
    if padcfg0_off + 4 > COMMUNITY_LEN || mmio.read::<u32>(padcfg0_off) == 0xffff_ffff {
        return None;
    }

    let is_off = TGL_GPI_IS + pad.reg_num as usize * 4;
    let ie_off = TGL_GPI_IE + pad.reg_num as usize * 4;
    let bit = 1u32 << pad.gpp_offset;

    // Idempotent: trigger/polarity from the GpioInt, IO-APIC routing, RX enabled.
    let mut cfg = mmio.read::<u32>(padcfg0_off);
    cfg &= !PADCFG0_RXEVCFG_MASK;
    if edge {
        cfg |= PADCFG0_RXEVCFG_EDGE;
    }
    if active_low {
        cfg |= PADCFG0_RXINV;
    } else {
        cfg &= !PADCFG0_RXINV;
    }
    cfg |= PADCFG0_GPIROUTIOXAPIC;
    cfg &= !PADCFG0_GPIORXDIS;
    mmio.write::<u32>(padcfg0_off, cfg);

    // Clear any stale status; leave the interrupt enable masked.
    mmio.write::<u32>(is_off, bit);
    let ie = mmio.read::<u32>(ie_off) & !bit;
    mmio.write::<u32>(ie_off, ie);

    PINCTRL.call_once(|| Pinctrl {
        mmio,
        padcfg0_off,
        is_off,
        ie_off,
        bit,
    });
    Some(pin)
}

/// Bring-up diagnostic: a firmware-routed touchpad pad has `GPIROUTIOXAPIC`
/// (bit 20) set, while all-ones or zero means the SoC constants are wrong.
pub fn padcfg0_snapshot() -> Option<u32> {
    let st = PINCTRL.get()?;
    Some(st.mmio.read::<u32>(st.padcfg0_off))
}

/// Enable the pad interrupt (clear stale status first).
pub fn pad_irq_unmask() {
    if let Some(st) = PINCTRL.get() {
        st.mmio.write::<u32>(st.is_off, st.bit);
        let ie = st.mmio.read::<u32>(st.ie_off) | st.bit;
        st.mmio.write::<u32>(st.ie_off, ie);
    }
}

pub fn pad_irq_mask() {
    if let Some(st) = PINCTRL.get() {
        let ie = st.mmio.read::<u32>(st.ie_off) & !st.bit;
        st.mmio.write::<u32>(st.ie_off, ie);
    }
}

/// IRQ context: masks and acknowledges a pending+enabled pad interrupt, and
/// returns `true` so the caller wakes the drain thread and EOIs. The mask holds
/// until that thread reads the report, so a still-asserted level line cannot
/// re-storm.
pub fn service_pending() -> bool {
    let Some(st) = PINCTRL.get() else {
        return false;
    };
    let is = st.mmio.read::<u32>(st.is_off);
    let ie = st.mmio.read::<u32>(st.ie_off);
    if is & ie & st.bit == 0 {
        return false;
    }
    st.mmio.write::<u32>(st.ie_off, ie & !st.bit);
    st.mmio.write::<u32>(st.is_off, st.bit); // write-1-to-clear
    true
}

#[doc(hidden)]
pub fn test_pin_for_crs_gpio_touchpad() -> slopos_testing::TestResult {
    // Touchpad GpioInt line 177 (ISH_GP_4) → pinctrl pin 116.
    match pin_for_crs_gpio(177) {
        Some(116) => slopos_testing::TestResult::Pass,
        _ => slopos_testing::TestResult::Fail,
    }
}

#[doc(hidden)]
pub fn test_pin_for_crs_gpio_bounds() -> slopos_testing::TestResult {
    // gpp group 2 covers lines 160..=180; one past the end is gpp 3's first.
    let ok = pin_for_crs_gpio(160) == Some(99)
        && pin_for_crs_gpio(180) == Some(119)
        && pin_for_crs_gpio(192) == Some(120) // gpp 3 base
        && pin_for_crs_gpio(95).is_none(); // below community 1
    if ok {
        slopos_testing::TestResult::Pass
    } else {
        slopos_testing::TestResult::Fail
    }
}

#[doc(hidden)]
pub fn test_resolve_pad_touchpad() -> slopos_testing::TestResult {
    // Pin 116: gpp 2, gpp_offset 17, padno 49 (116 - 67).
    match resolve_pad(116) {
        Some(p) if p.reg_num == 2 && p.gpp_offset == 17 && p.padno == 49 => {
            slopos_testing::TestResult::Pass
        }
        _ => slopos_testing::TestResult::Fail,
    }
}

#[doc(hidden)]
pub fn test_register_offsets_touchpad() -> slopos_testing::TestResult {
    // Pad 116 → GPI_IS = 0x100 + 2*4 = 0x108, GPI_IE = 0x120 + 8 = 0x128.
    let pad = match resolve_pad(116) {
        Some(p) => p,
        None => return slopos_testing::TestResult::Fail,
    };
    let is_off = TGL_GPI_IS + pad.reg_num as usize * 4;
    let ie_off = TGL_GPI_IE + pad.reg_num as usize * 4;
    if is_off == 0x108 && ie_off == 0x128 && (1u32 << pad.gpp_offset) == 1 << 17 {
        slopos_testing::TestResult::Pass
    } else {
        slopos_testing::TestResult::Fail
    }
}

slopos_testing::stest!(name = test_pin_for_crs_gpio_touchpad, suite = pinctrl);
slopos_testing::stest!(name = test_pin_for_crs_gpio_bounds, suite = pinctrl);
slopos_testing::stest!(name = test_resolve_pad_touchpad, suite = pinctrl);
slopos_testing::stest!(name = test_register_offsets_touchpad, suite = pinctrl);
