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
