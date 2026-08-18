//! HID-over-I²C transport: the register protocol an I²C-HID touchpad speaks,
//! over an [`I2cBus`]. Polled — no GPIO interrupt.

use slopos_ostd::KArc;
use slopos_ostd::{KVec, klog_info, klog_warn};

use crate::hpet;
use crate::i2c::{I2cBus, I2cError};

/// The 30-byte HID-over-I²C descriptor (little-endian fields).
#[derive(Clone, Copy, Debug, Default)]
pub struct HidDescriptor {
    pub hid_desc_length: u16,
    pub bcd_version: u16,
    pub report_desc_length: u16,
    pub report_desc_register: u16,
    pub input_register: u16,
    pub max_input_length: u16,
    pub output_register: u16,
    pub max_output_length: u16,
    pub command_register: u16,
    pub data_register: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    pub version_id: u16,
}

impl HidDescriptor {
    fn parse(raw: &[u8; 30]) -> Self {
        let w = |o: usize| u16::from_le_bytes([raw[o], raw[o + 1]]);
        Self {
            hid_desc_length: w(0),
            bcd_version: w(2),
            report_desc_length: w(4),
            report_desc_register: w(6),
            input_register: w(8),
            max_input_length: w(10),
            output_register: w(12),
            max_output_length: w(14),
            command_register: w(16),
            data_register: w(18),
            vendor_id: w(20),
            product_id: w(22),
            version_id: w(24),
        }
    }
}

const OPCODE_RESET: u8 = 0x01;
const OPCODE_SET_REPORT: u8 = 0x03;
const OPCODE_SET_POWER: u8 = 0x08;
const POWER_ON: u8 = 0x00;
/// Report type nibble for a Feature report in the SET_REPORT command byte.
const REPORT_TYPE_FEATURE: u8 = 0x03;

const HID_DESC_LEN: u16 = 30;
const HID_BCD_VERSION: u16 = 0x0100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HidError {
    Bus(I2cError),
    BadDescriptor,
    NoReportDescriptor,
    OutOfMemory,
}

impl From<I2cError> for HidError {
    fn from(e: I2cError) -> Self {
        HidError::Bus(e)
    }
}

pub struct I2cHid {
    bus: KArc<I2cBus>,
    addr: u8,
    desc: HidDescriptor,
}

impl I2cHid {
    pub fn descriptor(&self) -> &HidDescriptor {
        &self.desc
    }

    pub fn max_input_length(&self) -> usize {
        self.desc.max_input_length as usize
    }

    /// `desc_reg` is the HID descriptor register address obtained from ACPI
    /// `_DSM`.
    pub fn bring_up(bus: KArc<I2cBus>, addr: u8, desc_reg: u16) -> Result<Self, HidError> {
        let mut raw = [0u8; 30];
        bus.write_read(addr, &[desc_reg as u8, (desc_reg >> 8) as u8], &mut raw)?;
        let desc = HidDescriptor::parse(&raw);
        if desc.hid_desc_length != HID_DESC_LEN || desc.bcd_version != HID_BCD_VERSION {
            klog_warn!(
                "i2c-hid: bad descriptor len={:#x} bcd={:#x}",
                desc.hid_desc_length,
                desc.bcd_version
            );
            return Err(HidError::BadDescriptor);
        }
        klog_info!(
            "i2c-hid: descriptor ok vid={:#06x} pid={:#06x} max_in={} rdesc_len={}",
            desc.vendor_id,
            desc.product_id,
            desc.max_input_length,
            desc.report_desc_length
        );

        let dev = Self { bus, addr, desc };

        // Power-up ordering and delays are spec-mandated.
        dev.set_power(POWER_ON)?;
        hpet::delay_ms(60);
        dev.reset()?;
        // Polled bring-up: the spec allows a fixed wait in place of the
        // reset-complete interrupt.
        hpet::delay_ms(100);
        dev.set_power(POWER_ON)?;

        Ok(dev)
    }

    pub fn fetch_report_descriptor(&self) -> Result<KVec<u8>, HidError> {
        let len = self.desc.report_desc_length as usize;
        if len == 0 {
            return Err(HidError::NoReportDescriptor);
        }
        let mut buf = KVec::with_capacity(len).map_err(|_| HidError::OutOfMemory)?;
        buf.resize(len, 0u8).map_err(|_| HidError::OutOfMemory)?;
        let reg = self.desc.report_desc_register;
        self.bus.write_read(
            self.addr,
            &[reg as u8, (reg >> 8) as u8],
            buf.as_mut_slice(),
        )?;
        Ok(buf)
    }

    /// Returns payload bytes written to `out` — the report ID + data, with the
    /// 2-byte length prefix stripped — or 0 if nothing was pending.
    pub fn read_input_report(&self, out: &mut [u8]) -> Result<usize, HidError> {
        let max = self.desc.max_input_length as usize;
        if max < 2 {
            return Ok(0);
        }
        // 256 bounds the stack frame; the touchpad's max_input_length is well
        // under it.
        let mut buf = [0u8; 256];
        let n = max.min(buf.len());
        self.bus.read(self.addr, &mut buf[..n])?;
        let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        if len == 0 || len == 0xffff || len < 2 || len > n {
            return Ok(0); // nothing pending / sentinel
        }
        let body = &buf[2..len];
        let copy = body.len().min(out.len());
        out[..copy].copy_from_slice(&body[..copy]);
        Ok(copy)
    }

    /// The command goes to the command register, the report ID + payload to
    /// the data register, in one transaction. Report IDs must be below `0x0f`.
    pub fn set_feature_report(&self, report_id: u8, data: &[u8]) -> Result<(), HidError> {
        let cmd = self.desc.command_register;
        let dreg = self.desc.data_register;
        // data-register length field counts itself + the report ID + payload
        let wlen = 2 + 1 + data.len() as u16;
        let mut buf = [0u8; 16];
        let header = [
            cmd as u8,
            (cmd >> 8) as u8,
            (REPORT_TYPE_FEATURE << 4) | (report_id & 0x0f),
            OPCODE_SET_REPORT,
            dreg as u8,
            (dreg >> 8) as u8,
            wlen as u8,
            (wlen >> 8) as u8,
            report_id,
        ];
        if header.len() + data.len() > buf.len() {
            return Err(HidError::BadDescriptor);
        }
        buf[..header.len()].copy_from_slice(&header);
        buf[header.len()..header.len() + data.len()].copy_from_slice(data);
        self.bus
            .write(self.addr, &buf[..header.len() + data.len()])?;
        Ok(())
    }

    fn set_power(&self, state: u8) -> Result<(), HidError> {
        // SET_POWER takes a zero command byte and the power state after the
        // opcode.
        let cmd = self.desc.command_register;
        let buf = [
            cmd as u8,
            (cmd >> 8) as u8,
            0x00,
            OPCODE_SET_POWER,
            state & 0x0f,
        ];
        self.bus.write(self.addr, &buf)?;
        Ok(())
    }

    fn reset(&self) -> Result<(), HidError> {
        let cmd = self.desc.command_register;
        let buf = [cmd as u8, (cmd >> 8) as u8, 0x00, OPCODE_RESET];
        self.bus.write(self.addr, &buf)?;
        Ok(())
    }
}
