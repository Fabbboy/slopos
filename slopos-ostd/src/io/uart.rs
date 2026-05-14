//! Safe COM1-class 8250/16550 UART register window.
//!
//! Folds the per-call `unsafe { port.read() }` / `port.write()`
//! pattern from the serial driver behind a typed register-bank view.
//! Each method certifies the side-effect contract once (port I/O on a
//! UART register is benign — reads on RBR/LSR/IIR consume one byte from
//! the receive FIFO or return status bits; writes on THR/IER/LCR/MCR/FCR
//! drive the transmitter or configure the chip). Callers stay
//! `unsafe`-free.
//!
//! All accesses use the non-registry-gated
//! [`raw_port::Port`](crate::io::raw_port::Port) primitive so the
//! window remains reachable before
//! [`register_io_port_registry`](crate::io::port::register_io_port_registry).

use crate::io::port_consts::{
    UART_REG_IER, UART_REG_IIR, UART_REG_LCR, UART_REG_LSR, UART_REG_MCR, UART_REG_RBR,
    UART_REG_SCR,
};
use crate::io::raw_port::Port;

/// Typed handle to an 8250/16550-class UART register window.
///
/// Construction is `pub const fn new(base)`; the per-method `unsafe`
/// blocks are interior to this module so consumers stay unsafe-free.
#[derive(Clone, Copy)]
pub struct UartRegs {
    base: Port<u8>,
}

impl UartRegs {
    #[inline]
    pub const fn new(base: Port<u8>) -> Self {
        Self { base }
    }

    #[inline]
    pub const fn base_address(&self) -> u16 {
        self.base.address()
    }

    /// Read RBR (offset 0). Consumes one byte from the receive FIFO.
    #[inline]
    pub fn read_rbr(&self) -> u8 {
        // SAFETY: COM1-class RBR read is the standard pop-one-byte
        // primitive; no side effects beyond the FIFO advance the caller
        // intends.
        unsafe { self.base.offset(UART_REG_RBR).read() }
    }

    /// Read LSR (offset 5). Side-effect free.
    #[inline]
    pub fn read_lsr(&self) -> u8 {
        // SAFETY: LSR read is side-effect-free.
        unsafe { self.base.offset(UART_REG_LSR).read() }
    }

    /// Read IIR (offset 2).
    #[inline]
    pub fn read_iir(&self) -> u8 {
        // SAFETY: IIR read returns status bits; no side effects.
        unsafe { self.base.offset(UART_REG_IIR).read() }
    }

    /// Read SCR (offset 7). Scratchpad register — no side effects.
    #[inline]
    pub fn read_scr(&self) -> u8 {
        // SAFETY: SCR read is side-effect-free (scratchpad).
        unsafe { self.base.offset(UART_REG_SCR).read() }
    }

    /// Write IER (offset 1).
    #[inline]
    pub fn write_ier(&self, value: u8) {
        // SAFETY: writing IER toggles UART interrupt sources — the
        // intended side effect.
        unsafe { self.base.offset(UART_REG_IER).write(value) }
    }

    /// Write FCR (offset 2). FCR/IIR share the same offset; this is
    /// the write side.
    #[inline]
    pub fn write_fcr(&self, value: u8) {
        // SAFETY: FCR write configures the FIFO — the intended side
        // effect.
        unsafe { self.base.offset(UART_REG_IIR).write(value) }
    }

    /// Write LCR (offset 3).
    #[inline]
    pub fn write_lcr(&self, value: u8) {
        // SAFETY: LCR write sets line-control parameters.
        unsafe { self.base.offset(UART_REG_LCR).write(value) }
    }

    /// Write MCR (offset 4).
    #[inline]
    pub fn write_mcr(&self, value: u8) {
        // SAFETY: MCR write drives modem-control output lines.
        unsafe { self.base.offset(UART_REG_MCR).write(value) }
    }

    /// Write SCR (offset 7).
    #[inline]
    pub fn write_scr(&self, value: u8) {
        // SAFETY: SCR write stores a byte in the scratchpad — no
        // user-visible effect.
        unsafe { self.base.offset(UART_REG_SCR).write(value) }
    }

    /// Write RBR/THR/DLL (offset 0). Used during init for divisor-low
    /// programming when DLAB is set, and for raw byte transmit
    /// elsewhere (callers must already hold the appropriate lock).
    #[inline]
    pub fn write_rbr(&self, value: u8) {
        // SAFETY: RBR/THR write transmits one byte (or programs the
        // divisor when DLAB=1). Both are intended side effects.
        unsafe { self.base.offset(UART_REG_RBR).write(value) }
    }
}
