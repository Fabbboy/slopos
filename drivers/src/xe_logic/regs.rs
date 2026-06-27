//! Intel Gen12 / Display-IP-13 display-engine register map and field helpers.
//!
//! Pure data and `const fn` bit math: no MMIO, no I/O. The hardware-sequencing
//! half (the feature-gated `crate::xe` module) supplies the BAR0 base and turns
//! these offsets into reads/writes; everything here stays trivially testable.
//!
//! Offsets are byte offsets into BAR0 (GTTMMADR). Per-pipe registers follow the
//! SKL+ universal-plane convention of a fixed 0x1000 stride between pipes A/B/C,
//! so the canonical pipe-A absolute address plus `Pipe::stride_bytes()` yields
//! any pipe's register. Field values were cross-checked against the target
//! a7a8 (Alder/Raptor-Lake-P) silicon: a live `DSPACNTR` of 0x94009000 decodes
//! exactly to enable | XRGB8888 | render-decompression | Y-tiled.

// ---------------------------------------------------------------------------
// Global GTT (GTTMMADR aperture)
// ---------------------------------------------------------------------------

/// Byte offset of the Global GTT within BAR0. Registers occupy [0, 8 MiB); the
/// GGTT page-table occupies [8 MiB, 16 MiB) on this 16 MiB GTTMMADR.
pub const GTTMMADR_GGTT_OFFSET: usize = 0x800000;

/// Size of a single Gen12 GGTT page-table entry, in bytes.
pub const GGTT_PTE_BYTES: usize = 8;

/// GGTT table size for a 16 MiB GTTMMADR (this target): 8 MiB of PTEs.
pub const GGTT_TABLE_SIZE_8MB: usize = 8 * 1024 * 1024;

/// GGTT table size for an 8 MiB GTTMMADR variant: 4 MiB of PTEs.
pub const GGTT_TABLE_SIZE_4MB: usize = 4 * 1024 * 1024;

/// GGTT PTE present (valid) bit.
pub const GGTT_PTE_PRESENT: u64 = 1 << 0;

/// GGTT PTE physical-address field. The Gen12 GGTT only consumes bits [38:12]
/// of the address, but this conservative [51:12] mask is safe: the unused high
/// bits read back as zero, so masking never drops live address bits.
pub const GGTT_PTE_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

/// Graphics flush-control register (Gen6+). Writing [`GFX_FLSH_CNTL_EN`] after
/// updating GGTT page-table entries via MMIO invalidates the display engine's
/// GGTT TLB so it observes the new translations. It sits in the register half of
/// BAR0 (below [`GTTMMADR_GGTT_OFFSET`]).
pub const GFX_FLSH_CNTL_GEN6: usize = 0x101008;

/// Enable bit written to [`GFX_FLSH_CNTL_GEN6`] to flush the GGTT TLB.
pub const GFX_FLSH_CNTL_EN: u32 = 1 << 0;

// ---------------------------------------------------------------------------
// Pipe selection and per-pipe addressing
// ---------------------------------------------------------------------------

/// Address stride between corresponding registers of adjacent pipes.
pub const PIPE_REG_STRIDE: usize = 0x1000;

/// The three display pipes. Each maps to a contiguous block of plane, pipe,
/// and cursor registers separated from the next pipe by `PIPE_REG_STRIDE`.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Pipe {
    A = 0,
    B = 1,
    C = 2,
}

impl Pipe {
    /// All pipes in hardware order, for iteration in callers and tests.
    pub const ALL: [Pipe; 3] = [Pipe::A, Pipe::B, Pipe::C];

    /// Zero-based hardware index (A=0, B=1, C=2).
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Byte distance from a pipe-A register to this pipe's matching register.
    pub const fn stride_bytes(self) -> usize {
        self.index() * PIPE_REG_STRIDE
    }
}

/// Add a pipe's stride to a pipe-A absolute register offset.
pub const fn pipe_relative(pipe_a_offset: usize, pipe: Pipe) -> usize {
    pipe_a_offset + pipe.stride_bytes()
}

// ---------------------------------------------------------------------------
// Primary (universal) plane registers — pipe-A absolutes
//
// The plane register group begins at 0x70180 for pipe A; offsets within the
// group: CTL +0x00, STRIDE +0x08, POS +0x0c, SIZE +0x10, KEYVAL +0x14,
// KEYMSK +0x18, SURF +0x1c, KEYMAX +0x20, OFFSET/TILEOFF +0x24,
// AUX_DIST +0x40, AUX_OFFSET +0x44, COLOR_CTL +0x4c.
// ---------------------------------------------------------------------------

pub const PLANE_CTL_A: usize = 0x70180;
pub const PLANE_STRIDE_A: usize = 0x70188;
pub const PLANE_POS_A: usize = 0x7018c;
pub const PLANE_SIZE_A: usize = 0x70190;
pub const PLANE_KEYVAL_A: usize = 0x70194;
pub const PLANE_KEYMSK_A: usize = 0x70198;
pub const PLANE_SURF_A: usize = 0x7019c;
pub const PLANE_KEYMAX_A: usize = 0x701a0;
/// Plane tile/linear offset register (TILEOFF / PLANE_OFFSET).
pub const PLANE_OFFSET_A: usize = 0x701a4;
pub const PLANE_AUX_DIST_A: usize = 0x701c0;
pub const PLANE_AUX_OFFSET_A: usize = 0x701c4;
pub const PLANE_COLOR_CTL_A: usize = 0x701cc;
/// Watermark level 0 (`PLANE_WM_1_A_0`); levels 1..=7 follow at +4 each.
pub const PLANE_WM_A: usize = 0x70240;
/// Transition watermark (`PLANE_WM_TRANS_1_A`).
pub const PLANE_WM_TRANS_A: usize = 0x70268;
/// DBUF/DDB allocation (`PLANE_BUF_CFG_1_A`).
pub const PLANE_BUF_CFG_A: usize = 0x7027c;

/// Number of per-plane watermark levels the display engine programs on Gen12
/// (`PLANE_WM` levels 0..=7).
pub const PLANE_WM_LEVELS: usize = 8;

pub const fn plane_ctl(pipe: Pipe) -> usize {
    pipe_relative(PLANE_CTL_A, pipe)
}

pub const fn plane_stride(pipe: Pipe) -> usize {
    pipe_relative(PLANE_STRIDE_A, pipe)
}

pub const fn plane_pos(pipe: Pipe) -> usize {
    pipe_relative(PLANE_POS_A, pipe)
}

pub const fn plane_size(pipe: Pipe) -> usize {
    pipe_relative(PLANE_SIZE_A, pipe)
}

pub const fn plane_keyval(pipe: Pipe) -> usize {
    pipe_relative(PLANE_KEYVAL_A, pipe)
}

pub const fn plane_keymsk(pipe: Pipe) -> usize {
    pipe_relative(PLANE_KEYMSK_A, pipe)
}

pub const fn plane_surf(pipe: Pipe) -> usize {
    pipe_relative(PLANE_SURF_A, pipe)
}

pub const fn plane_keymax(pipe: Pipe) -> usize {
    pipe_relative(PLANE_KEYMAX_A, pipe)
}

pub const fn plane_offset(pipe: Pipe) -> usize {
    pipe_relative(PLANE_OFFSET_A, pipe)
}

pub const fn plane_aux_dist(pipe: Pipe) -> usize {
    pipe_relative(PLANE_AUX_DIST_A, pipe)
}

pub const fn plane_aux_offset(pipe: Pipe) -> usize {
    pipe_relative(PLANE_AUX_OFFSET_A, pipe)
}

pub const fn plane_color_ctl(pipe: Pipe) -> usize {
    pipe_relative(PLANE_COLOR_CTL_A, pipe)
}

/// Watermark register for `level` (0..[`PLANE_WM_LEVELS`)); per-level stride 4 B.
pub const fn plane_wm(pipe: Pipe, level: usize) -> usize {
    pipe_relative(PLANE_WM_A + level * 4, pipe)
}

pub const fn plane_wm_trans(pipe: Pipe) -> usize {
    pipe_relative(PLANE_WM_TRANS_A, pipe)
}

pub const fn plane_buf_cfg(pipe: Pipe) -> usize {
    pipe_relative(PLANE_BUF_CFG_A, pipe)
}

// ---------------------------------------------------------------------------
// Pipe / transcoder registers — pipe-A absolutes
// ---------------------------------------------------------------------------

/// Pipe display scanline (read-only current scanline counter).
pub const PIPEDSL_A: usize = 0x70000;
/// Pipe configuration (a.k.a. TRANSCONF on Gen12): enable bit31, active bit30.
pub const PIPECONF_A: usize = 0x70008;
/// Pipe source size: `((width-1) << 16) | (height-1)`.
pub const PIPESRC_A: usize = 0x6001c;

pub const fn pipe_dsl(pipe: Pipe) -> usize {
    pipe_relative(PIPEDSL_A, pipe)
}

pub const fn pipe_conf(pipe: Pipe) -> usize {
    pipe_relative(PIPECONF_A, pipe)
}

pub const fn pipe_src(pipe: Pipe) -> usize {
    pipe_relative(PIPESRC_A, pipe)
}

// ---------------------------------------------------------------------------
// Cursor plane registers — pipe-A absolutes
// ---------------------------------------------------------------------------

pub const CUR_CTL_A: usize = 0x70080;
pub const CUR_BASE_A: usize = 0x70084;
pub const CUR_POS_A: usize = 0x70088;

pub const fn cur_ctl(pipe: Pipe) -> usize {
    pipe_relative(CUR_CTL_A, pipe)
}

pub const fn cur_base(pipe: Pipe) -> usize {
    pipe_relative(CUR_BASE_A, pipe)
}

pub const fn cur_pos(pipe: Pipe) -> usize {
    pipe_relative(CUR_POS_A, pipe)
}

// ---------------------------------------------------------------------------
// Cursor DBUF/DDB + watermark registers — pipe-A absolutes
//
// The cursor is a real plane in the Gen12 DBUF/watermark model: enabling it with
// a zero DDB allocation (CUR_BUF_CFG = 0) or zero watermark starves the pipe's
// fetch and corrupts the PRIMARY plane.
// ---------------------------------------------------------------------------

/// Cursor watermark level 0 (`CUR_WM_A_0`); levels 1..=7 follow at +4 each.
pub const CUR_WM_A: usize = 0x70140;
/// Cursor transition watermark (`CUR_WM_TRANS_A`).
pub const CUR_WM_TRANS_A: usize = 0x70168;
/// Cursor DBUF/DDB allocation (`CUR_BUF_CFG_A`).
pub const CUR_BUF_CFG_A: usize = 0x7017c;

/// Cursor watermark register for `level` (0..[`PLANE_WM_LEVELS`)); stride 4 B.
pub const fn cur_wm(pipe: Pipe, level: usize) -> usize {
    pipe_relative(CUR_WM_A + level * 4, pipe)
}

pub const fn cur_wm_trans(pipe: Pipe) -> usize {
    pipe_relative(CUR_WM_TRANS_A, pipe)
}

pub const fn cur_buf_cfg(pipe: Pipe) -> usize {
    pipe_relative(CUR_BUF_CFG_A, pipe)
}

// ---------------------------------------------------------------------------
// DDB allocation + watermark bitfields (PLANE_BUF_CFG / CUR_BUF_CFG share one
// layout, as do PLANE_WM / CUR_WM): the DDB register stores START (first block)
// and END (LAST block, i.e. end-exclusive minus one); the WM register carries an
// enable, an ignore-lines bit, a lines field, and a blocks field.
// ---------------------------------------------------------------------------

/// DDB START field, bits [12:0].
pub const DDB_BUF_START_MASK: u32 = 0x0000_1fff;
/// DDB END field, bits [28:16]. Holds the inclusive last block (end-exclusive
/// minus one).
pub const DDB_BUF_END_MASK: u32 = 0x1fff_0000;

/// Watermark enable, bit 31.
pub const WM_ENABLE: u32 = bit(31);
/// Watermark ignore-lines, bit 30: the level is satisfied by the blocks field
/// alone, the lines field is unused.
pub const WM_IGNORE_LINES: u32 = bit(30);
/// Watermark lines field, bits [26:14].
pub const WM_LINES_MASK: u32 = 0x07ff_c000;
/// Watermark blocks field, bits [11:0].
pub const WM_BLOCKS_MASK: u32 = 0x0000_0fff;

// ---------------------------------------------------------------------------
// CUR_CTL (CURACNTR) cursor-mode + CUR_POS (CURAPOS) position bitfields
//
// The mode field selects the cursor size and pixel format, the 0x20 bit promotes
// a fixed-format size code to 32-bpp ARGB, and CUR_POS packs a sign-magnitude
// (x, y) so a negative coordinate places the cursor partly off the top/left.
// ---------------------------------------------------------------------------

/// Cursor disabled: the mode field is zero.
pub const MCURSOR_MODE_DISABLE: u32 = 0x00;

/// Mode-field mask covering bit 5 and bits [2:0].
pub const MCURSOR_MODE_MASK: u32 = 0x27;

/// ARGB-format promote bit (bit 5): ORed onto a size code to select 32-bpp ARGB.
pub const MCURSOR_MODE_ARGB: u32 = bit(5);

/// 32-bpp fixed-format size codes (the low three bits of the mode field).
pub const MCURSOR_MODE_64_32B_AX: u32 = 0x07;
pub const MCURSOR_MODE_128_32B_AX: u32 = 0x02;
pub const MCURSOR_MODE_256_32B_AX: u32 = 0x03;

/// 32-bpp ARGB cursor modes: a 64/128/256 size code ORed with the ARGB bit
/// (= 0x27 / 0x22 / 0x23).
pub const MCURSOR_MODE_64_ARGB_AX: u32 = MCURSOR_MODE_ARGB | MCURSOR_MODE_64_32B_AX;
pub const MCURSOR_MODE_128_ARGB_AX: u32 = MCURSOR_MODE_ARGB | MCURSOR_MODE_128_32B_AX;
pub const MCURSOR_MODE_256_ARGB_AX: u32 = MCURSOR_MODE_ARGB | MCURSOR_MODE_256_32B_AX;

/// Cursor fetch-arbitration slot field, bits [30:28]. On display IP version 13
/// (Alder Lake-P / Raptor Lake-P) this MUST be programmed to 1 — workaround
/// Wa_22012358565. With the field left at 0 the cursor plane's memory-fetch
/// arbitration starves and corrupts the PRIMARY plane's fetch, so the primary
/// decodes its linear surface with the X-tile (512-byte) stride unit and
/// replicates 8x vertically (512/64). Applies to display IP version 13 only.
pub const MCURSOR_ARB_SLOTS_MASK: u32 = 0x7000_0000;

/// Place `slots` into the cursor arbitration-slot field.
pub const fn mcursor_arb_slots(slots: u32) -> u32 {
    reg_field_set(MCURSOR_ARB_SLOTS_MASK, slots)
}

/// CUR_POS X magnitude field, bits [14:0].
pub const CURSOR_POS_X_MASK: u32 = 0x0000_7fff;
/// CUR_POS X sign bit (bit 15): set marks a negative X (partly off-screen left).
pub const CURSOR_POS_X_SIGN: u32 = bit(15);
/// CUR_POS Y magnitude field, bits [30:16].
pub const CURSOR_POS_Y_MASK: u32 = 0x7fff_0000;
/// CUR_POS Y sign bit (bit 31): set marks a negative Y (partly off-screen top).
pub const CURSOR_POS_Y_SIGN: u32 = bit(31);

// ---------------------------------------------------------------------------
// Diagnostic-only registers
// ---------------------------------------------------------------------------

/// Transcoder DDI function control — pipe-A absolute, per-pipe stride.
pub const TRANS_DDI_FUNC_CTL_A: usize = 0x60400;

pub const fn trans_ddi_func_ctl(pipe: Pipe) -> usize {
    pipe_relative(TRANS_DDI_FUNC_CTL_A, pipe)
}

/// DDI A buffer control (eDP-1 on this panel drives DDI A).
pub const DDI_BUF_CTL_A: usize = 0x64000;
/// PCH panel-power status.
pub const PCH_PP_STATUS: usize = 0xc7200;
/// PCH panel-power control.
pub const PCH_PP_CONTROL: usize = 0xc7204;
/// Power-well control 2 (display power wells).
pub const PWR_WELL_CTL2: usize = 0x45404;

// ---------------------------------------------------------------------------
// PLANE_CTL bitfields (SKL+ / Gen12 universal plane)
// ---------------------------------------------------------------------------

/// Plane enable.
pub const PLANE_CTL_ENABLE: u32 = bit(31);

/// YUV range-correction disable. Always set for RGB source pixels (the live
/// `DSPACNTR=0x94009000` carries it on an XRGB8888 plane).
pub const PLANE_CTL_YUV_RANGE_CORRECTION_DISABLE: u32 = bit(28);

/// Low bit of the pixel-format field.
pub const PLANE_CTL_FORMAT_SHIFT: u32 = 24;
/// Pixel-format field, bits [27:24].
pub const PLANE_CTL_FORMAT_MASK: u32 = 0xf << PLANE_CTL_FORMAT_SHIFT;
/// 8:8:8:8 RGB format code (0b0100). The silicon uses one code for both
/// XRGB8888 and ARGB8888; whether the alpha channel participates in blending is
/// selected separately (alpha-mode / PLANE_COLOR_CTL), and channel order is
/// picked by `PLANE_CTL_COLOR_ORDER_RGBX`, so both aliases resolve here.
pub const PLANE_CTL_FORMAT_XRGB8888: u32 = 4 << PLANE_CTL_FORMAT_SHIFT;
pub const PLANE_CTL_FORMAT_ARGB8888: u32 = PLANE_CTL_FORMAT_XRGB8888;

/// Render-decompression (CCS) enable. Set means the surface is render-compressed.
pub const PLANE_CTL_RENDER_DECOMP_ENABLE: u32 = bit(15);

/// Channel order: set selects RGBX; clear is the default BGRX that matches an
/// ARGB8888 little-endian framebuffer (bytes B, G, R, X).
pub const PLANE_CTL_COLOR_ORDER_RGBX: u32 = bit(20);

/// Low bit of the tiling-mode field.
pub const PLANE_CTL_TILING_SHIFT: u32 = 10;
/// Tiling-mode field, bits [12:10].
pub const PLANE_CTL_TILING_MASK: u32 = 0x7 << PLANE_CTL_TILING_SHIFT;
pub const PLANE_CTL_TILING_LINEAR: u32 = 0 << PLANE_CTL_TILING_SHIFT;
pub const PLANE_CTL_TILING_X: u32 = 1 << PLANE_CTL_TILING_SHIFT;
pub const PLANE_CTL_TILING_Y: u32 = 4 << PLANE_CTL_TILING_SHIFT;
pub const PLANE_CTL_TILING_YF: u32 = 5 << PLANE_CTL_TILING_SHIFT;

// ---------------------------------------------------------------------------
// PIPECONF / TRANSCONF bits
// ---------------------------------------------------------------------------

/// Pipe enable.
pub const PIPECONF_ENABLE: u32 = bit(31);
/// Pipe state active (hardware reports the pipe is actually driving output).
pub const PIPECONF_STATE_ACTIVE: u32 = bit(30);

// ---------------------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------------------

/// A single set bit at `shift`.
pub const fn bit(shift: u32) -> u32 {
    1u32 << shift
}

/// Extract `mask`'s field from `value`, shifted down to bit 0.
pub const fn reg_field_get(mask: u32, value: u32) -> u32 {
    (value & mask) >> mask.trailing_zeros()
}

/// Place `value` into `mask`'s field. Inverse of `reg_field_get`; bits of
/// `value` that do not fit the field are masked off.
pub const fn reg_field_set(mask: u32, value: u32) -> u32 {
    (value << mask.trailing_zeros()) & mask
}
