//! Synopsys DesignWare APB I²C master controller (Intel LPSS variant).
//!
//! The MMIO window (one PCI BAR) is sub-divided by the LPSS wrapper:
//!   `+0x000` DesignWare core registers (`IC_*`)
//!   `+0x200` LPSS private shim (reset / clock divider / address remap)
//!   `+0x800` integrated DMA engine (unused — this driver is PIO-only)
//!
//! The transfer engine is **polled** (busy-waits on the status registers);
//! no controller interrupt is required.
//!
//! All hardware access goes through the [`Mmio32`] trait so the transfer
//! FSM can be exercised against a scripted mock in the test harness.

#![allow(dead_code)]

// =============================================================================
// DesignWare core register offsets (relative to the BAR base + 0x000)
// =============================================================================

const IC_CON: usize = 0x00;
const IC_TAR: usize = 0x04;
const IC_DATA_CMD: usize = 0x10;
const IC_SS_SCL_HCNT: usize = 0x14;
const IC_SS_SCL_LCNT: usize = 0x18;
const IC_FS_SCL_HCNT: usize = 0x1c;
const IC_FS_SCL_LCNT: usize = 0x20;
const IC_INTR_STAT: usize = 0x2c;
const IC_INTR_MASK: usize = 0x30;
const IC_RAW_INTR_STAT: usize = 0x34;
const IC_RX_TL: usize = 0x38;
const IC_TX_TL: usize = 0x3c;
const IC_CLR_INTR: usize = 0x40;
const IC_CLR_TX_ABRT: usize = 0x54;
const IC_ENABLE: usize = 0x6c;
const IC_STATUS: usize = 0x70;
const IC_TXFLR: usize = 0x74;
const IC_RXFLR: usize = 0x78;
const IC_SDA_HOLD: usize = 0x7c;
const IC_TX_ABRT_SOURCE: usize = 0x80;
const IC_ENABLE_STATUS: usize = 0x9c;
const IC_COMP_PARAM_1: usize = 0xf4;
const IC_COMP_VERSION: usize = 0xf8;
const IC_COMP_TYPE: usize = 0xfc;

/// `IC_COMP_TYPE` magic identifying a DesignWare I²C core ("DW" + 0x0140).
const DW_IC_COMP_TYPE_VALUE: u32 = 0x4457_0140;
/// `IC_COMP_VERSION` from which the `IC_SDA_HOLD` register is valid ("1.11*").
const DW_IC_SDA_HOLD_MIN_VERS: u32 = 0x3131_312A;

// IC_CON bits.
const IC_CON_MASTER: u32 = 1 << 0;
const IC_CON_SPEED_STD: u32 = 1 << 1;
const IC_CON_SPEED_FAST: u32 = 2 << 1;
const IC_CON_RESTART_EN: u32 = 1 << 5;
const IC_CON_SLAVE_DISABLE: u32 = 1 << 6;
/// Master, fast-mode (400 kHz), repeated-START enabled, slave port off.
const IC_CON_MASTER_FAST: u32 =
    IC_CON_MASTER | IC_CON_SPEED_FAST | IC_CON_RESTART_EN | IC_CON_SLAVE_DISABLE;

// IC_DATA_CMD bits (above the 8-bit data field).
const IC_DATA_CMD_READ: u32 = 1 << 8;
const IC_DATA_CMD_STOP: u32 = 1 << 9;
const IC_DATA_CMD_RESTART: u32 = 1 << 10;
const IC_DATA_CMD_DAT_MASK: u32 = 0xff;

// IC_RAW_INTR_STAT / IC_INTR_STAT bits we poll.
const IC_INTR_TX_ABRT: u32 = 1 << 6;
const IC_INTR_STOP_DET: u32 = 1 << 9;

// IC_ENABLE / IC_ENABLE_STATUS bits.
const IC_ENABLE_ENABLE: u32 = 1 << 0;
const IC_ENABLE_ABORT: u32 = 1 << 1;
const IC_ENABLE_STATUS_EN: u32 = 1 << 0;

// IC_TX_ABRT_SOURCE bits we decode into errors.
const ABRT_7B_ADDR_NOACK: u32 = 1 << 0;
const ABRT_TXDATA_NOACK: u32 = 1 << 3;
const ABRT_ARB_LOST: u32 = 1 << 12;

// =============================================================================
// LPSS private shim (relative to the BAR base + 0x200)
// =============================================================================

const LPSS_PRIV_BASE: usize = 0x200;
const LPSS_PRIV_CLK_DIV: usize = LPSS_PRIV_BASE + 0x00;
const LPSS_PRIV_RESETS: usize = LPSS_PRIV_BASE + 0x04;
const LPSS_PRIV_REMAP_LO: usize = LPSS_PRIV_BASE + 0x40;
const LPSS_PRIV_REMAP_HI: usize = LPSS_PRIV_BASE + 0x44;

/// Deassert both the function and iDMA resets (`FUNC=0x3 | IDMA=BIT2`).
const LPSS_RESETS_DEASSERT: u32 = 0x7;

// LPSS clock-divider register (at the start of the private window). The
// DesignWare APB registers are clocked by this; if it's gated, `IC_COMP_TYPE`
// reads back as 0. Layout: bit31 = enable, [30:16] = N, [15:1] = M.
const LPSS_CLK_ENABLE: u32 = 1 << 31;
/// M (numerator, [15:1]) | N (denominator, [30:16]).
const LPSS_CLK_MN_MASK: u32 = 0x7fff_fffe;
/// 1:1 passthrough (M = N = 1) — only used if firmware left M/N cleared.
const LPSS_CLK_MN_1_1: u32 = (1 << 1) | (1 << 16);

// =============================================================================
// Timing
// =============================================================================

/// Default functional ("ic_clk") frequency in kHz. SCL high/low counts are
/// derived from this; recompute via [`scl_counts`] for a different rate.
const DEFAULT_IC_CLK_KHZ: u32 = 133_000;

/// Bus-line fall time (ns) used when computing SCL counts.
const FALL_NS: u32 = 300;

// Fast-mode (400 kHz) I²C symbol timings (ns).
const FS_THIGH_NS: u32 = 600;
const FS_TLOW_NS: u32 = 1300;
// Standard-mode (100 kHz) I²C symbol timings (ns).
const SS_THIGH_NS: u32 = 4000;
const SS_TLOW_NS: u32 = 4700;

/// Compute the DesignWare SCL high/low counter pair for one mode:
///
/// `hcnt = round(ic_clk_khz * (thigh + tf) / 1e6) - 3`
/// `lcnt = round(ic_clk_khz * (tlow  + tf) / 1e6) - 1`
///
/// Saturating so a bad clock never underflows into a huge count.
pub fn scl_counts(ic_clk_khz: u32, thigh_ns: u32, tlow_ns: u32, fall_ns: u32) -> (u16, u16) {
    let div_round = |clk: u32, t: u32| -> u32 {
        let num = (clk as u64) * (t as u64);
        ((num + 500_000) / 1_000_000) as u32
    };
    let hcnt = div_round(ic_clk_khz, thigh_ns + fall_ns).saturating_sub(3);
    let lcnt = div_round(ic_clk_khz, tlow_ns + fall_ns).saturating_sub(1);
    (hcnt.min(0xffff) as u16, lcnt.min(0xffff) as u16)
}

// =============================================================================
// Public types
// =============================================================================

/// 32-bit MMIO register accessor. Abstracts [`IoMem`] so the transfer
/// FSM is testable against a mock.
///
/// [`IoMem`]: slopos_ostd::mm::io_mem::IoMem
pub trait Mmio32 {
    fn r32(&self, off: usize) -> u32;
    fn w32(&self, off: usize, val: u32);
}

impl Mmio32 for slopos_mm::mmio::MmioRegion {
    #[inline]
    fn r32(&self, off: usize) -> u32 {
        self.read::<u32>(off)
    }
    #[inline]
    fn w32(&self, off: usize, val: u32) {
        self.write::<u32>(off, val);
    }
}

/// One leg of an I²C transaction at a single slave address. Consecutive
/// segments are separated by a repeated-START; a STOP is emitted after
/// the final byte of the final segment.
pub enum I2cSegment<'a> {
    /// Bytes to transmit.
    Write(&'a [u8]),
    /// Buffer to fill with received bytes.
    Read(&'a mut [u8]),
}

/// Failure modes of an I²C transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum I2cError {
    /// `IC_COMP_TYPE` did not read back the DesignWare magic — the BAR is
    /// not a DesignWare I²C core.
    NotDesignWare,
    /// The slave did not ACK its address (device absent / unpowered).
    AddrNack,
    /// A transmitted data byte was not ACKed.
    DataNack,
    /// Arbitration lost (multi-master collision).
    ArbLost,
    /// Generic transfer abort (see `IC_TX_ABRT_SOURCE`).
    Abort,
    /// A status poll timed out.
    Timeout,
    /// An empty transaction (no segments).
    Empty,
}

/// Real-time bound for a single status-poll wait. A working transfer
/// completes well within this; a wedged controller fails fast instead of
/// monopolising the CPU.
const I2C_TIMEOUT_MS: u32 = 50;
/// Iteration cap used alongside the time bound as a fallback.
const SPIN_CAP: u32 = 50_000_000;

/// HPET tick at which an `I2C_TIMEOUT_MS` wait gives up.
fn poll_deadline() -> u64 {
    crate::hpet::read_counter()
        .wrapping_add(crate::hpet::ms_to_ticks(I2C_TIMEOUT_MS).unwrap_or(u64::MAX))
}

#[inline]
fn poll_expired(deadline: u64) -> bool {
    crate::hpet::read_counter() >= deadline
}

/// A bound-up DesignWare I²C master over some [`Mmio32`] window.
pub struct DesignWareI2c<M: Mmio32> {
    mmio: M,
    tx_fifo_depth: u16,
    rx_fifo_depth: u16,
    ic_clk_khz: u32,
}

impl<M: Mmio32> DesignWareI2c<M> {
    /// Wrap a mapped controller window. Call [`init`](Self::init) before
    /// transferring.
    pub fn new(mmio: M) -> Self {
        Self {
            mmio,
            tx_fifo_depth: 8,
            rx_fifo_depth: 8,
            ic_clk_khz: DEFAULT_IC_CLK_KHZ,
        }
    }

    /// Override the assumed functional clock (kHz) used for SCL timing.
    pub fn set_ic_clk_khz(&mut self, khz: u32) {
        self.ic_clk_khz = khz;
    }

    /// Release the LPSS wrapper from reset and point its address-remap
    /// register at the controller's own physical base. PIO-only, so the
    /// clock divider and iDMA are left as firmware configured them.
    pub fn lpss_bringup(&self, mmio_phys_base: u64) {
        // Hold in reset, then deassert function + iDMA reset.
        self.mmio.w32(LPSS_PRIV_RESETS, 0);
        self.mmio.w32(LPSS_PRIV_RESETS, LPSS_RESETS_DEASSERT);
        // Ensure the LPSS functional clock is ungated — the DesignWare APB
        // registers (incl. IC_COMP_TYPE) read back as 0 if it's gated. Leave
        // firmware's M/N divider in place unless it left both fields zero, in
        // which case force a 1:1 passthrough so the divider doesn't stall.
        let mut div = self.mmio.r32(LPSS_PRIV_CLK_DIV);
        if div & LPSS_CLK_ENABLE == 0 {
            if div & LPSS_CLK_MN_MASK == 0 {
                div |= LPSS_CLK_MN_1_1;
            }
            div |= LPSS_CLK_ENABLE;
            self.mmio.w32(LPSS_PRIV_CLK_DIV, div);
        }
        // Tell the core its own MMIO base (REMAP; required even for PIO).
        self.mmio
            .w32(LPSS_PRIV_REMAP_LO, (mmio_phys_base & 0xffff_ffff) as u32);
        self.mmio
            .w32(LPSS_PRIV_REMAP_HI, (mmio_phys_base >> 32) as u32);
    }

    /// Verify the core, read FIFO depths, and program master/fast-mode
    /// config + SCL timing. Leaves the controller disabled and interrupts
    /// masked (this driver polls).
    pub fn init(&mut self) -> Result<(), I2cError> {
        let comp = self.mmio.r32(IC_COMP_TYPE);
        if comp != DW_IC_COMP_TYPE_VALUE {
            // 0xFFFFFFFF → still in D3 / decode off; 0x00000000 → clock gated;
            // 0x40145700 → byte-swapped core (not expected on x86 LPSS).
            slopos_ostd::klog_warn!(
                "i2c-dw: COMP_TYPE {:#010x} != {:#010x} (not powered/clocked or wrong device)",
                comp,
                DW_IC_COMP_TYPE_VALUE
            );
            return Err(I2cError::NotDesignWare);
        }

        self.disable()?;

        let param1 = self.mmio.r32(IC_COMP_PARAM_1);
        self.tx_fifo_depth = (((param1 >> 16) & 0xff) + 1) as u16;
        self.rx_fifo_depth = (((param1 >> 8) & 0xff) + 1) as u16;

        self.mmio.w32(IC_CON, IC_CON_MASTER_FAST);

        let (fs_hcnt, fs_lcnt) = scl_counts(self.ic_clk_khz, FS_THIGH_NS, FS_TLOW_NS, FALL_NS);
        let (ss_hcnt, ss_lcnt) = scl_counts(self.ic_clk_khz, SS_THIGH_NS, SS_TLOW_NS, FALL_NS);
        self.mmio.w32(IC_FS_SCL_HCNT, fs_hcnt as u32);
        self.mmio.w32(IC_FS_SCL_LCNT, fs_lcnt as u32);
        self.mmio.w32(IC_SS_SCL_HCNT, ss_hcnt as u32);
        self.mmio.w32(IC_SS_SCL_LCNT, ss_lcnt as u32);

        // Mask all interrupts; we poll IC_RAW_INTR_STAT.
        self.mmio.w32(IC_INTR_MASK, 0);
        // Single-entry FIFO thresholds — simplest correct polling behaviour.
        self.mmio.w32(IC_TX_TL, 0);
        self.mmio.w32(IC_RX_TL, 0);

        Ok(())
    }

    fn disable(&self) -> Result<(), I2cError> {
        self.mmio.w32(IC_ENABLE, 0);
        let deadline = poll_deadline();
        let mut cap = SPIN_CAP;
        while self.mmio.r32(IC_ENABLE_STATUS) & IC_ENABLE_STATUS_EN != 0 {
            cap -= 1;
            if cap == 0 || poll_expired(deadline) {
                return Err(I2cError::Timeout);
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    fn enable(&self) {
        self.mmio.w32(IC_ENABLE, IC_ENABLE_ENABLE);
    }

    /// Map an `IC_TX_ABRT_SOURCE` value to the most specific error.
    fn abort_error(src: u32) -> I2cError {
        if src & ABRT_7B_ADDR_NOACK != 0 {
            I2cError::AddrNack
        } else if src & ABRT_TXDATA_NOACK != 0 {
            I2cError::DataNack
        } else if src & ABRT_ARB_LOST != 0 {
            I2cError::ArbLost
        } else {
            I2cError::Abort
        }
    }

    /// Check for and clear a transfer abort. Returns the decoded error if
    /// one occurred.
    fn check_abort(&self) -> Option<I2cError> {
        if self.mmio.r32(IC_RAW_INTR_STAT) & IC_INTR_TX_ABRT == 0 {
            return None;
        }
        let src = self.mmio.r32(IC_TX_ABRT_SOURCE);
        // Reading IC_CLR_TX_ABRT clears the abort and unfreezes the TX FIFO.
        let _ = self.mmio.r32(IC_CLR_TX_ABRT);
        Some(Self::abort_error(src))
    }

    /// Perform one transaction: all segments to `addr`, repeated-START
    /// between segments, STOP after the last byte. Interleaves FIFO fill
    /// and drain, so transfers larger than the FIFO are handled.
    pub fn xfer(&self, addr: u8, segs: &mut [I2cSegment<'_>]) -> Result<(), I2cError> {
        let total_cmds: usize = segs.iter().map(|s| s.len()).sum();
        if total_cmds == 0 {
            return Err(I2cError::Empty);
        }
        let total_reads: usize = segs
            .iter()
            .map(|s| match s {
                I2cSegment::Read(b) => b.len(),
                I2cSegment::Write(_) => 0,
            })
            .sum();

        // The target address can only be programmed while disabled.
        self.disable()?;
        self.mmio.w32(IC_TAR, addr as u32 & 0x7f);
        self.enable();
        // Dummy reads to clear any stale interrupt/abort latches.
        let _ = self.mmio.r32(IC_CLR_INTR);

        // TX cursor over every byte of every segment.
        let mut tx_seg = 0usize;
        let mut tx_byte = 0usize;
        let mut cmds_left = total_cmds;
        // RX cursor over read-segment bytes only.
        let mut rx_seg = 0usize;
        let mut rx_byte = 0usize;
        let mut reads_left = total_reads;
        // Read commands pushed but not yet drained (bounded by RX FIFO).
        let mut rx_outstanding: usize = 0;

        let mut deadline = poll_deadline();
        let mut cap = SPIN_CAP;
        while cmds_left > 0 || reads_left > 0 {
            if let Some(e) = self.check_abort() {
                return Err(e);
            }

            // Fill: push command words while the TX FIFO has room and we
            // won't overflow the RX FIFO with pending read results.
            let tx_room = self
                .tx_fifo_depth
                .saturating_sub(self.mmio.r32(IC_TXFLR) as u16);
            let mut pushed = false;
            for _ in 0..tx_room {
                if cmds_left == 0 {
                    break;
                }
                // Advance over fully-consumed segments.
                while tx_seg < segs.len() && tx_byte >= segs[tx_seg].len() {
                    tx_seg += 1;
                    tx_byte = 0;
                }
                if tx_seg >= segs.len() {
                    break;
                }
                let is_read = matches!(segs[tx_seg], I2cSegment::Read(_));
                if is_read && rx_outstanding >= self.rx_fifo_depth as usize {
                    break; // would overflow RX FIFO; drain first
                }

                let first_of_seg = tx_byte == 0;
                let last_overall = cmds_left == 1;
                let mut cmd = 0u32;
                if first_of_seg && tx_seg != 0 {
                    cmd |= IC_DATA_CMD_RESTART;
                }
                if last_overall {
                    cmd |= IC_DATA_CMD_STOP;
                }
                match &segs[tx_seg] {
                    I2cSegment::Write(buf) => {
                        cmd |= buf[tx_byte] as u32 & IC_DATA_CMD_DAT_MASK;
                    }
                    I2cSegment::Read(_) => {
                        cmd |= IC_DATA_CMD_READ;
                        rx_outstanding += 1;
                    }
                }
                self.mmio.w32(IC_DATA_CMD, cmd);
                tx_byte += 1;
                cmds_left -= 1;
                pushed = true;
            }

            // Drain: pull received bytes into the read segments in order.
            let mut drained = false;
            let mut rx_avail = self.mmio.r32(IC_RXFLR);
            while rx_avail > 0 && reads_left > 0 {
                let byte = (self.mmio.r32(IC_DATA_CMD) & IC_DATA_CMD_DAT_MASK) as u8;
                while rx_seg < segs.len()
                    && (matches!(segs[rx_seg], I2cSegment::Write(_))
                        || rx_byte >= segs[rx_seg].len())
                {
                    if matches!(segs[rx_seg], I2cSegment::Read(_)) && rx_byte >= segs[rx_seg].len()
                    {
                        // finished this read segment
                    }
                    rx_seg += 1;
                    rx_byte = 0;
                }
                if let Some(I2cSegment::Read(buf)) = segs.get_mut(rx_seg) {
                    buf[rx_byte] = byte;
                    rx_byte += 1;
                }
                reads_left -= 1;
                rx_outstanding = rx_outstanding.saturating_sub(1);
                rx_avail -= 1;
                drained = true;
            }

            if !pushed && !drained {
                cap -= 1;
                if cap == 0 || poll_expired(deadline) {
                    return Err(I2cError::Timeout);
                }
                core::hint::spin_loop();
            } else {
                deadline = poll_deadline();
                cap = SPIN_CAP;
            }
        }

        // Wait for the STOP to land, watching for a late abort.
        let deadline = poll_deadline();
        let mut cap = SPIN_CAP;
        loop {
            if let Some(e) = self.check_abort() {
                return Err(e);
            }
            if self.mmio.r32(IC_RAW_INTR_STAT) & IC_INTR_STOP_DET != 0 {
                break;
            }
            cap -= 1;
            if cap == 0 || poll_expired(deadline) {
                return Err(I2cError::Timeout);
            }
            core::hint::spin_loop();
        }
        let _ = self.mmio.r32(IC_CLR_INTR);
        Ok(())
    }

    /// Convenience: write `tx`, repeated-START, read into `rx`. This is
    /// the shape every register-addressed I²C-HID read uses.
    pub fn write_read(&self, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), I2cError> {
        let mut segs = [I2cSegment::Write(tx), I2cSegment::Read(rx)];
        self.xfer(addr, &mut segs)
    }

    /// Convenience: write `tx` then STOP.
    pub fn write(&self, addr: u8, tx: &[u8]) -> Result<(), I2cError> {
        let mut segs = [I2cSegment::Write(tx)];
        self.xfer(addr, &mut segs)
    }

    /// Convenience: bare read into `rx` (no preceding register address) —
    /// the I²C-HID input-report read.
    pub fn read(&self, addr: u8, rx: &mut [u8]) -> Result<(), I2cError> {
        let mut segs = [I2cSegment::Read(rx)];
        self.xfer(addr, &mut segs)
    }
}

impl I2cSegment<'_> {
    fn len(&self) -> usize {
        match self {
            I2cSegment::Write(b) => b.len(),
            I2cSegment::Read(b) => b.len(),
        }
    }
}
