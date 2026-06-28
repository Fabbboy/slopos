//! Parser for the bytes of an ACPI `ResourceTemplate` buffer — just the
//! descriptors the touchpad enumeration needs: the I²C serial-bus
//! connection (slave address + controller path + bus speed) and the
//! GpioInt connection (interrupt pin + polarity).

use slopos_ostd::KVec;

use super::object::bytes_from_slice;

/// Parsed I²C serial-bus connection descriptor.
pub struct I2cResource {
    /// 7-bit (or 10-bit) slave address.
    pub slave_addr: u16,
    /// Bus speed in Hz (`ConnectionSpeed`).
    pub speed_hz: u32,
    /// ACPI path of the controller, e.g. `\_SB.PC00.I2C1`.
    pub controller: KVec<u8>,
}

/// Parsed I/O-port resource descriptor (small tag 0x08 `IO`, or 0x09 `FixedIO`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoPortResource {
    /// Base I/O port (the descriptor's range minimum).
    pub base: u16,
    /// Number of ports the window covers.
    pub len: u8,
}

/// Parsed IRQ resource descriptor (small tag 0x04).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqResource {
    /// Bitmask of IRQ lines this descriptor can use (bit `n` = IRQ `n`).
    pub irq_mask: u16,
    /// `true` = edge triggered, `false` = level.
    pub edge: bool,
    /// `true` = active-low / falling.
    pub active_low: bool,
}

impl IrqResource {
    /// The lowest IRQ line in the mask (the single line in a fixed descriptor
    /// like the keyboard's `IRQ {1}`), if any bit is set.
    pub fn first_line(&self) -> Option<u8> {
        if self.irq_mask == 0 {
            None
        } else {
            Some(self.irq_mask.trailing_zeros() as u8)
        }
    }
}

/// Parsed GpioInt connection descriptor.
pub struct GpioIntResource {
    /// First pin number in the descriptor's pin table.
    pub pin: u16,
    /// `true` = edge triggered, `false` = level.
    pub edge: bool,
    /// `true` = active-low / falling.
    pub active_low: bool,
    /// ACPI path of the GPIO controller, e.g. `\_SB.GPI0`.
    pub controller: KVec<u8>,
}

const TAG_END: u8 = 0x79;
const LARGE_TYPE_GPIO: u8 = 0x0c;
const LARGE_TYPE_SERIAL_BUS: u8 = 0x0e;
const SERIAL_BUS_TYPE_I2C: u8 = 0x01;

// Small resource descriptor "names" (bits 6:3 of the tag byte).
const SMALL_TYPE_IRQ: u8 = 0x04;
const SMALL_TYPE_IO: u8 = 0x08;
const SMALL_TYPE_FIXED_IO: u8 = 0x09;

/// Collect every I/O-port descriptor (`IO` / `FixedIO`) in a `_CRS`/`_PRS`
/// resource template buffer, in order.
pub fn parse_io_ports(buf: &[u8]) -> KVec<IoPortResource> {
    let mut out = KVec::new();
    each_small(buf, |typ, body, len| {
        match typ {
            // I/O Port Descriptor: info(1), min(2), max(2), align(1), length(1).
            SMALL_TYPE_IO if len >= 7 => {
                let base = u16::from_le_bytes([buf[body + 1], buf[body + 2]]);
                let length = buf[body + 6];
                let _ = out.push(IoPortResource { base, len: length });
            }
            // Fixed Location I/O Port Descriptor: base(2), length(1).
            SMALL_TYPE_FIXED_IO if len >= 3 => {
                let base = u16::from_le_bytes([buf[body], buf[body + 1]]);
                let length = buf[body + 2];
                let _ = out.push(IoPortResource { base, len: length });
            }
            _ => {}
        }
    });
    out
}

/// Collect every IRQ descriptor (small tag 0x04) in a resource template buffer.
pub fn parse_irqs(buf: &[u8]) -> KVec<IrqResource> {
    let mut out = KVec::new();
    each_small(buf, |typ, body, len| {
        if typ == SMALL_TYPE_IRQ && len >= 2 {
            let irq_mask = u16::from_le_bytes([buf[body], buf[body + 1]]);
            // Optional 3rd "IRQ Information" byte: bit0 = edge(1)/level(0),
            // bit3 = active-low(1)/active-high(0). Absent ⇒ ISA default
            // (edge, active-high).
            let (edge, active_low) = if len >= 3 {
                let info = buf[body + 2];
                (info & 0x01 != 0, info & 0x08 != 0)
            } else {
                (true, false)
            };
            let _ = out.push(IrqResource {
                irq_mask,
                edge,
                active_low,
            });
        }
    });
    out
}

/// Iterate small resource descriptors, calling `f(type, body_start, body_len)`
/// for each. Large descriptors are skipped; iteration stops at the End tag or
/// the first malformed/truncated descriptor.
fn each_small(buf: &[u8], mut f: impl FnMut(u8, usize, usize)) {
    let mut p = 0usize;
    while p < buf.len() {
        let tag = buf[p];
        if tag == TAG_END {
            break;
        }
        if tag & 0x80 == 0 {
            // Small descriptor: bits 6:3 = type, bits 2:0 = body length.
            let len = (tag & 0x07) as usize;
            let typ = (tag >> 3) & 0x0f;
            let body = p + 1;
            if body + len > buf.len() {
                break;
            }
            f(typ, body, len);
            p = body + len;
        } else {
            // Large descriptor: 2-byte length follows the tag.
            let Some(&lo) = buf.get(p + 1) else { break };
            let Some(&hi) = buf.get(p + 2) else { break };
            let len = u16::from_le_bytes([lo, hi]) as usize;
            p += 3 + len;
        }
    }
}

/// Find and parse the first I²C serial-bus descriptor in `buf`.
pub fn parse_i2c(buf: &[u8]) -> Option<I2cResource> {
    each_large(buf, |typ, p, total| {
        if typ != LARGE_TYPE_SERIAL_BUS {
            return None;
        }
        if *buf.get(p + 5)? != SERIAL_BUS_TYPE_I2C {
            return None;
        }
        let tdl = u16::from_le_bytes([*buf.get(p + 10)?, *buf.get(p + 11)?]) as usize;
        let speed = u32::from_le_bytes([
            *buf.get(p + 12)?,
            *buf.get(p + 13)?,
            *buf.get(p + 14)?,
            *buf.get(p + 15)?,
        ]);
        let addr = u16::from_le_bytes([*buf.get(p + 16)?, *buf.get(p + 17)?]);
        // Type-specific data starts at byte 12; ResourceSource follows it.
        let src_start = p + 12 + tdl;
        let controller = read_cstr(buf, src_start, p + total);
        Some(I2cResource {
            slave_addr: addr,
            speed_hz: speed,
            controller,
        })
    })
}

/// Find and parse the first GpioInt descriptor in `buf`.
pub fn parse_gpio_int(buf: &[u8]) -> Option<GpioIntResource> {
    each_large(buf, |typ, p, total| {
        if typ != LARGE_TYPE_GPIO {
            return None;
        }
        // Byte 4: connection type (0 = interrupt).
        if *buf.get(p + 4)? != 0 {
            return None;
        }
        let int_flags = u16::from_le_bytes([*buf.get(p + 7)?, *buf.get(p + 8)?]);
        let edge = int_flags & 0x01 != 0;
        let active_low = (int_flags >> 1) & 0x03 == 1;
        let pin_tbl_off = u16::from_le_bytes([*buf.get(p + 14)?, *buf.get(p + 15)?]) as usize;
        let src_off = u16::from_le_bytes([*buf.get(p + 17)?, *buf.get(p + 18)?]) as usize;
        let pin = u16::from_le_bytes([*buf.get(p + pin_tbl_off)?, *buf.get(p + pin_tbl_off + 1)?]);
        let controller = read_cstr(buf, p + src_off, p + total);
        Some(GpioIntResource {
            pin,
            edge,
            active_low,
            controller,
        })
    })
}

/// Iterate large resource descriptors, calling `f(type, abs_start, total_len)`
/// and returning its first `Some`. Small descriptors are skipped.
fn each_large<T>(buf: &[u8], mut f: impl FnMut(u8, usize, usize) -> Option<T>) -> Option<T> {
    let mut p = 0usize;
    while p < buf.len() {
        let tag = buf[p];
        if tag == TAG_END {
            break;
        }
        if tag & 0x80 == 0 {
            // Small resource: low 3 bits = body length.
            let len = (tag & 0x07) as usize;
            p += 1 + len;
            continue;
        }
        let len = u16::from_le_bytes([*buf.get(p + 1)?, *buf.get(p + 2)?]) as usize;
        let total = 3 + len;
        let typ = tag & 0x7f;
        if let Some(v) = f(typ, p, total) {
            return Some(v);
        }
        p += total;
    }
    None
}

fn read_cstr(buf: &[u8], start: usize, end: usize) -> KVec<u8> {
    let mut q = start;
    let mut s = &buf[0..0];
    while q < end && q < buf.len() {
        if buf[q] == 0 {
            s = &buf[start..q];
            break;
        }
        q += 1;
    }
    if s.is_empty() && start < end.min(buf.len()) {
        s = &buf[start..end.min(buf.len())];
    }
    bytes_from_slice(s)
}
