//! Pure cursor-plane encoding for the hardware cursor: bit math over plain data,
//! no MMIO. Field values come from the register constants in [`super::regs`], so
//! the register map stays the single source of truth.

use super::regs::{
    self, MCURSOR_MODE_64_ARGB_AX, MCURSOR_MODE_128_ARGB_AX, MCURSOR_MODE_256_ARGB_AX,
    MCURSOR_MODE_DISABLE,
};

/// The ARGB cursor sizes the Gen12 cursor plane supports, as a square edge
/// length in pixels; each maps to one `MCURSOR_MODE_*_ARGB_AX` code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMode {
    Argb64,
    Argb128,
    Argb256,
}

/// The smallest supported ARGB cursor mode whose square covers a `side`x`side`
/// image; `None` for a zero side or one past the 256x256 hardware maximum.
pub const fn mode_for_side(side: u32) -> Option<CursorMode> {
    match side {
        1..=64 => Some(CursorMode::Argb64),
        65..=128 => Some(CursorMode::Argb128),
        129..=256 => Some(CursorMode::Argb256),
        _ => None,
    }
}

/// The `CUR_CTL` (CURACNTR) value for `mode` on a `display_ip_version` display
/// engine. Display IP version 13 (ADL-P / RPL-P) also ORs in
/// `MCURSOR_ARB_SLOTS(1)` — workaround Wa_22012358565 — without which the
/// cursor's fetch arbitration starves the primary plane.
pub const fn cur_ctl_value(mode: CursorMode, enable: bool, display_ip_version: u8) -> u32 {
    if !enable {
        return MCURSOR_MODE_DISABLE;
    }
    let mode_bits = match mode {
        CursorMode::Argb64 => MCURSOR_MODE_64_ARGB_AX,
        CursorMode::Argb128 => MCURSOR_MODE_128_ARGB_AX,
        CursorMode::Argb256 => MCURSOR_MODE_256_ARGB_AX,
    };
    let arb_slots = if display_ip_version == 13 {
        regs::mcursor_arb_slots(1)
    } else {
        0
    };
    mode_bits | arb_slots
}

/// Pack a signed `(x, y)` cursor position into the `CUR_POS` (CURAPOS) layout:
/// magnitude of x in bits [14:0] and of y in bits [30:16], each with its sign bit
/// ([15] / [31]) set when negative. The hardware packs sign-plus-magnitude here,
/// not two's complement.
pub const fn cur_pos_pack(x: i32, y: i32) -> u32 {
    let (x_magnitude, x_sign) = split_sign(x, regs::CURSOR_POS_X_SIGN);
    let (y_magnitude, y_sign) = split_sign(y, regs::CURSOR_POS_Y_SIGN);
    regs::reg_field_set(regs::CURSOR_POS_X_MASK, x_magnitude)
        | x_sign
        | regs::reg_field_set(regs::CURSOR_POS_Y_MASK, y_magnitude)
        | y_sign
}

/// Split a signed coordinate into its field magnitude and the sign bit to OR in.
/// `unsigned_abs` so `i32::MIN` cannot overflow the negation.
const fn split_sign(value: i32, sign_bit: u32) -> (u32, u32) {
    if value < 0 {
        (value.unsigned_abs(), sign_bit)
    } else {
        (value as u32, 0)
    }
}
