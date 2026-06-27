//! Pure cursor-plane encoding for the hardware cursor.
//!
//! Plain bit math over plain data: no MMIO, no allocation, no I/O. The
//! hardware-sequencing half (the cursor module of `crate::xe`) supplies the
//! register window and turns these values into CUR_CTL / CUR_POS writes. Field
//! values come straight from the register constants in [`super::regs`], so the
//! register map stays the single source of truth and these routines remain
//! trivially testable without a GPU. Every routine is a total `const fn`: no
//! panics, no allocation, evaluable at compile time.

use super::regs::{
    self, MCURSOR_MODE_64_ARGB_AX, MCURSOR_MODE_128_ARGB_AX, MCURSOR_MODE_256_ARGB_AX,
    MCURSOR_MODE_DISABLE,
};

/// The ARGB cursor sizes the Gen12 cursor plane supports, as a square edge
/// length in pixels. Each maps to one `MCURSOR_MODE_*_ARGB_AX` code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMode {
    Argb64,
    Argb128,
    Argb256,
}

/// The smallest supported ARGB cursor mode whose square covers a `side`x`side`
/// image, or `None` when the side is zero (no image) or exceeds the largest
/// (256x256) hardware cursor.
pub const fn mode_for_side(side: u32) -> Option<CursorMode> {
    match side {
        1..=64 => Some(CursorMode::Argb64),
        65..=128 => Some(CursorMode::Argb128),
        129..=256 => Some(CursorMode::Argb256),
        _ => None,
    }
}

/// The `CUR_CTL` (CURACNTR) value for `mode` on a `display_ip_version` display
/// engine: the matching ARGB mode code when `enable` is set, or the disable code
/// (mode field cleared) when it is not. Beyond the mode field, display IP version
/// 13 ORs in `MCURSOR_ARB_SLOTS(1)` — workaround Wa_22012358565 — without which
/// the cursor's fetch arbitration corrupts the primary plane (see
/// [`super::regs::MCURSOR_ARB_SLOTS_MASK`]). No pipe-CSC or gamma bit is touched.
pub const fn cur_ctl_value(mode: CursorMode, enable: bool, display_ip_version: u8) -> u32 {
    if !enable {
        return MCURSOR_MODE_DISABLE;
    }
    let mode_bits = match mode {
        CursorMode::Argb64 => MCURSOR_MODE_64_ARGB_AX,
        CursorMode::Argb128 => MCURSOR_MODE_128_ARGB_AX,
        CursorMode::Argb256 => MCURSOR_MODE_256_ARGB_AX,
    };
    // Wa_22012358565: display IP version 13 (ADL-P / RPL-P) must program one
    // cursor arbitration slot, or the cursor starves the primary plane's fetch.
    let arb_slots = if display_ip_version == 13 {
        regs::mcursor_arb_slots(1)
    } else {
        0
    };
    mode_bits | arb_slots
}

/// Pack a signed `(x, y)` cursor position into the `CUR_POS` (CURAPOS) layout:
/// the magnitude of x in bits [14:0] and of y in bits [30:16], each with its
/// sign bit ([15] / [31]) set when the coordinate is negative. A negative
/// coordinate places the cursor partly off the top/left edge. The hardware uses
/// sign-plus-magnitude packing here, not two's complement.
pub const fn cur_pos_pack(x: i32, y: i32) -> u32 {
    let (x_magnitude, x_sign) = split_sign(x, regs::CURSOR_POS_X_SIGN);
    let (y_magnitude, y_sign) = split_sign(y, regs::CURSOR_POS_Y_SIGN);
    regs::reg_field_set(regs::CURSOR_POS_X_MASK, x_magnitude)
        | x_sign
        | regs::reg_field_set(regs::CURSOR_POS_Y_MASK, y_magnitude)
        | y_sign
}

/// Split a signed coordinate into its field magnitude and the sign bit to OR in.
/// Uses `unsigned_abs` so `i32::MIN` cannot overflow the negation.
const fn split_sign(value: i32, sign_bit: u32) -> (u32, u32) {
    if value < 0 {
        (value.unsigned_abs(), sign_bit)
    } else {
        (value as u32, 0)
    }
}
