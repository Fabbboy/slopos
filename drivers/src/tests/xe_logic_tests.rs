//! Regression tests for `crate::xe_logic` — pure logic, no GPU required, so these
//! run in the standard `just test` suite even on a QEMU that cannot emulate the
//! Intel display engine.
//!
//! Coverage mirrors the four pure modules plus the register helpers: PCI-ID →
//! platform identification, `PLANE_CTL`/size/stride decode-encode round-trips
//! (anchored on the live `DSPACNTR=0x94009000` captured from the target a7a8),
//! GGTT page-table-entry math and above-firmware placement, the `xe.*` cmdline
//! parser, and the per-pipe register offset/field helpers.

use slopos_abi::PixelFormat;
use slopos_testing::{TestResult, assert_eq_test, assert_test, fail, pass};

use crate::xe_logic::{cmdline, cursor_config, ddb, ggtt_pte, plane_config, platform, regs};

// The firmware framebuffer the target's UEFI/Limine GOP leaves live: PLANE_SURF
// (DSPASURF) at GGTT byte 0x0112c000, spanning a 1920x1080 XRGB8888 surface
// (rounded generously to 8 MiB here for the placement tests).
const FW_SURF_GGTT: u64 = 0x0112_c000;
const FW_SURF_LEN: u64 = 8 * 1024 * 1024;
// A 16 MiB GTTMMADR carries an 8 MiB GGTT table, mapping a 4 GiB GPU VA space.
const GGTT_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

// =============================================================================
// platform: PCI Device-ID → display platform
// =============================================================================

/// The target laptop's iGPU (8086:a7a8) identifies as an Alder/Raptor-Lake-P
/// Gen13 display, IP version 13, using the SKL+ universal-plane conventions.
pub fn xe_platform_identifies_target_a7a8() -> TestResult {
    assert_test!(platform::is_intel_vendor(0x8086));
    assert_test!(!platform::is_intel_vendor(0x1234));

    let plat = match platform::identify(0x8086, 0xa7a8) {
        Some(p) => p,
        None => return fail!("a7a8 must identify as a supported display"),
    };
    assert_eq_test!(plat.generation, platform::XeDisplayGen::Gen13);
    assert_eq_test!(plat.display_ip_version(), 13u8);
    assert_test!(plat.uses_skl_universal_plane_regs());
    pass!()
}

/// A representative spread of known Gen12 / Gen13 Device IDs resolve to the
/// correct display-IP generation.
pub fn xe_platform_known_dids() -> TestResult {
    // Tiger Lake-U GT2 and Alder Lake-S — Gen12, display IP version 12.
    for did in [0x9a49u16, 0x4680u16] {
        match platform::identify(0x8086, did) {
            Some(p) => {
                assert_eq_test!(p.generation, platform::XeDisplayGen::Gen12);
                assert_eq_test!(p.display_ip_version(), 12u8);
            }
            None => return fail!("Device ID {:#06x} should be a known Gen12 part", did),
        }
    }
    // Alder Lake-P GT2 and a Raptor-Lake-P A7xx sibling — Gen13, IP version 13.
    for did in [0x46a8u16, 0xa720u16] {
        match platform::identify(0x8086, did) {
            Some(p) => {
                assert_eq_test!(p.generation, platform::XeDisplayGen::Gen13);
                assert_eq_test!(p.display_ip_version(), 13u8);
            }
            None => return fail!("Device ID {:#06x} should be a known Gen13 part", did),
        }
    }
    // The display-generation tag agrees with its own version helper.
    assert_eq_test!(platform::XeDisplayGen::Gen12.display_ip_version(), 12u8);
    assert_eq_test!(platform::XeDisplayGen::Gen13.display_ip_version(), 13u8);
    assert_test!(platform::XeDisplayGen::Gen13.uses_skl_universal_plane_regs());
    pass!()
}

/// An unknown Device ID and any non-Intel vendor both fail identification.
pub fn xe_platform_unknown_did_is_none() -> TestResult {
    assert_test!(
        platform::identify(0x8086, 0xffff).is_none(),
        "unknown Intel Device ID must not match"
    );
    assert_test!(
        platform::identify(0x1234, 0xa7a8).is_none(),
        "a7a8 under a non-Intel vendor must not match"
    );
    pass!()
}

// =============================================================================
// plane_config: PLANE_CTL / PLANE_SIZE / PLANE_POS / PLANE_STRIDE
// =============================================================================

/// `PLANE_SIZE` and `PLANE_POS` survive encode → decode for live-panel cases.
pub fn xe_plane_size_pos_roundtrip() -> TestResult {
    for (w, h) in [(1920u32, 1080u32), (800u32, 600u32), (1u32, 1u32)] {
        assert_eq_test!(
            plane_config::decode_size(plane_config::encode_size(w, h)),
            (w, h)
        );
    }
    // PLANE_SIZE biases each dimension by one: 1x1 encodes to all-zero.
    assert_eq_test!(plane_config::encode_size(1, 1), 0u32);
    assert_eq_test!(plane_config::decode_size(0), (1u32, 1u32));

    for (x, y) in [(0u32, 0u32), (64u32, 48u32), (1919u32, 1079u32)] {
        assert_eq_test!(
            plane_config::decode_pos(plane_config::encode_pos(x, y)),
            (x, y)
        );
    }
    pass!()
}

/// The live `DSPACNTR=0x94009000` decodes to enable | XRGB8888 | Y-tiled |
/// render-decompressed | BGRX order, matching the captured silicon state.
pub fn xe_plane_decode_live_dspacntr() -> TestResult {
    let ctl = plane_config::decode_ctl(0x9400_9000);
    assert_test!(ctl.enable, "live plane must read back enabled");
    assert_eq_test!(ctl.format, plane_config::PlaneFormat::Rgb8888);
    assert_eq_test!(ctl.tiling, plane_config::Tiling::YTiled);
    assert_eq_test!(ctl.color_order, plane_config::ColorOrder::Bgrx);
    assert_test!(ctl.render_decompressed, "live plane is render-compressed");

    // The full snapshot assembled from the live register read-back: 1920x1080
    // at GGTT 0x0112c000, Y-tiled stride unit 0x3c.
    let cfg = plane_config::PlaneConfig::from_registers(
        0x9400_9000,
        plane_config::encode_size(1920, 1080),
        plane_config::encode_pos(0, 0),
        0x3c,
        0x0112_c000,
    );
    assert_test!(cfg.enable);
    assert_eq_test!(cfg.width, 1920u32);
    assert_eq_test!(cfg.height, 1080u32);
    assert_eq_test!(cfg.tiling, plane_config::Tiling::YTiled);
    assert_eq_test!(cfg.stride_reg, 0x3cu32);
    assert_eq_test!(cfg.surf_ggtt, 0x0112_c000u32);
    pass!()
}

/// The linear repoint target keeps format/order/enable while forcing linear
/// tiling and clearing render-decompression.
pub fn xe_plane_encode_ctl_linear() -> TestResult {
    let repoint = plane_config::encode_ctl_linear(
        plane_config::PlaneFormat::Rgb8888,
        plane_config::ColorOrder::Bgrx,
        true,
    );
    let back = plane_config::decode_ctl(repoint);
    assert_test!(back.enable, "repoint target must enable the plane");
    assert_eq_test!(back.format, plane_config::PlaneFormat::Rgb8888);
    assert_eq_test!(back.tiling, plane_config::Tiling::Linear);
    assert_test!(
        !back.render_decompressed,
        "linear target must not be compressed"
    );
    assert_eq_test!(back.color_order, plane_config::ColorOrder::Bgrx);

    // RGBX order and a disabled plane survive the same round-trip.
    let rgbx_off = plane_config::decode_ctl(plane_config::encode_ctl_linear(
        plane_config::PlaneFormat::Rgb8888,
        plane_config::ColorOrder::Rgbx,
        false,
    ));
    assert_test!(!rgbx_off.enable);
    assert_eq_test!(rgbx_off.color_order, plane_config::ColorOrder::Rgbx);
    assert_eq_test!(rgbx_off.tiling, plane_config::Tiling::Linear);
    pass!()
}

/// ABI pixel formats map to a plane format only for the 32-bit RGB codes; the
/// 24-bit packed formats cannot be scanned out directly.
pub fn xe_plane_pixel_format_mapping() -> TestResult {
    for fmt in [
        PixelFormat::Argb8888,
        PixelFormat::Xrgb8888,
        PixelFormat::Rgba8888,
        PixelFormat::Bgra8888,
    ] {
        assert_test!(
            plane_config::pixel_format_to_plane(fmt).is_some(),
            "32-bit RGB format must map to a plane format"
        );
        assert_eq_test!(
            plane_config::PlaneFormat::from_pixel_format(fmt),
            Some(plane_config::PlaneFormat::Rgb8888)
        );
    }
    assert_test!(plane_config::pixel_format_to_plane(PixelFormat::Rgb888).is_none());
    assert_test!(plane_config::pixel_format_to_plane(PixelFormat::Bgr888).is_none());
    assert_test!(plane_config::PlaneFormat::from_pixel_format(PixelFormat::Rgb888).is_none());

    // The 8:8:8:8 code collapses to one canonical opaque ABI format.
    assert_eq_test!(
        plane_config::PlaneFormat::Rgb8888.to_pixel_format(),
        Some(PixelFormat::Xrgb8888)
    );
    // Placed-bits round-trip: Xrgb8888 → field bits → back to a known format.
    let bits = match plane_config::pixel_format_to_plane(PixelFormat::Xrgb8888) {
        Some(b) => b,
        None => return fail!("Xrgb8888 must produce placed plane-format bits"),
    };
    assert_test!(plane_config::plane_to_pixel_format(bits).is_some());
    // The 0b0100 format field decodes back to Rgb8888 and a bogus field does not.
    assert_eq_test!(
        plane_config::PlaneFormat::from_field(4),
        plane_config::PlaneFormat::Rgb8888
    );
    assert_eq_test!(plane_config::PlaneFormat::Rgb8888.to_field(), 4u32);
    pass!()
}

/// Linear `PLANE_STRIDE` is a count of 64-byte units and round-trips with the
/// byte pitch.
pub fn xe_plane_linear_stride() -> TestResult {
    // 1920 * 4 bytes per scanline = 7680 bytes = 120 units of 64 bytes.
    assert_eq_test!(plane_config::linear_stride_reg(7680), 120u32);
    assert_eq_test!(plane_config::linear_stride_bytes(120), 7680u32);
    assert_eq_test!(
        plane_config::linear_stride_bytes(plane_config::linear_stride_reg(7680)),
        7680u32
    );
    pass!()
}

// =============================================================================
// ggtt_pte: page-table-entry encoding + above-firmware placement
// =============================================================================

/// A GGTT PTE folds in the page-frame address with the present bit; an
/// unaligned physical address is rejected.
pub fn xe_ggtt_pte_encode() -> TestResult {
    let pte = match ggtt_pte::pte_encode(0x0112_c000) {
        Some(p) => p,
        None => return fail!("aligned phys must encode"),
    };
    assert_test!(pte & regs::GGTT_PTE_PRESENT != 0, "present bit must be set");
    assert_eq_test!(pte & regs::GGTT_PTE_ADDR_MASK, 0x0112_c000u64);
    // A sub-page-aligned physical address cannot be mapped.
    assert_test!(
        ggtt_pte::pte_encode(0x0112_c001).is_none(),
        "unaligned phys must be rejected"
    );
    assert_test!(ggtt_pte::pte_encode(0xfff).is_none());
    pass!()
}

/// `ggtt_byte_offset` and `entry_index` are inverses for page-aligned inputs.
pub fn xe_ggtt_offset_index_inverse() -> TestResult {
    for index in [0u32, 1u32, 2048u32, 0x10_0000u32] {
        assert_eq_test!(
            ggtt_pte::entry_index(ggtt_pte::ggtt_byte_offset(index)),
            index
        );
    }
    // 8 MiB of GPU VA is entry 0x800 (2048).
    assert_eq_test!(ggtt_pte::entry_index(0x0080_0000), 2048u32);
    assert_eq_test!(ggtt_pte::ggtt_byte_offset(2048), 0x0080_0000u64);
    pass!()
}

/// Half-open region overlap: intersecting ranges overlap, touching/disjoint and
/// zero-length ranges do not.
pub fn xe_ggtt_region_overlaps() -> TestResult {
    assert_test!(
        ggtt_pte::region_overlaps(0, 0x1000, 0x800, 0x1000),
        "intersecting ranges must overlap"
    );
    assert_test!(
        !ggtt_pte::region_overlaps(0, 0x1000, 0x1000, 0x1000),
        "touching-at-a-boundary must not overlap"
    );
    assert_test!(
        !ggtt_pte::region_overlaps(0, 0x1000, 0x4000, 0x1000),
        "disjoint ranges must not overlap"
    );
    assert_test!(
        !ggtt_pte::region_overlaps(0, 0, 0, 0x1000),
        "a zero-length range covers no bytes"
    );
    // A region landing inside the firmware framebuffer extent is flagged.
    assert_test!(ggtt_pte::region_overlaps(
        FW_SURF_GGTT + 0x1000,
        0x1000,
        FW_SURF_GGTT,
        FW_SURF_LEN
    ));
    pass!()
}

/// `alloc_above` places a request strictly above the firmware surface (never
/// overlapping it) and refuses a request that cannot fit in the GGTT.
pub fn xe_ggtt_alloc_above() -> TestResult {
    let pages = 1024u32; // 4 MiB of scanout pages.
    let size = (pages as u64) * ggtt_pte::PAGE_SIZE_BYTES;
    let start =
        match ggtt_pte::alloc_above(FW_SURF_GGTT, FW_SURF_LEN, 0x1000, pages, GGTT_TOTAL_BYTES) {
            Some(s) => s,
            None => return fail!("the request must fit above the firmware surface"),
        };
    assert_test!(
        start >= FW_SURF_GGTT + FW_SURF_LEN,
        "placement must sit at or above the firmware extent's end"
    );
    assert_test!(
        !ggtt_pte::region_overlaps(start, size, FW_SURF_GGTT, FW_SURF_LEN),
        "placement must not overlap the firmware framebuffer"
    );
    // A GGTT far too small for the firmware extent plus the request fails.
    assert_test!(
        ggtt_pte::alloc_above(FW_SURF_GGTT, FW_SURF_LEN, 0x1000, pages, 0x10_0000).is_none(),
        "an over-large request must be rejected"
    );
    // Zero alignment is invalid.
    assert_test!(
        ggtt_pte::alloc_above(FW_SURF_GGTT, FW_SURF_LEN, 0, pages, GGTT_TOTAL_BYTES).is_none()
    );
    pass!()
}

// =============================================================================
// cmdline: xe.* knob parser
// =============================================================================

/// An empty command line yields the default configuration: the driver drives
/// the display (modeset on), diagnostics off, everything else auto.
pub fn xe_cmdline_defaults() -> TestResult {
    let cfg = cmdline::parse("");
    assert_eq_test!(cfg, cmdline::XeConfig::default());
    assert_test!(!cfg.diag, "diagnostics default off");
    assert_test!(
        cfg.modeset,
        "modeset defaults on so the driver drives scanout"
    );
    assert_eq_test!(cfg.pipe, None);
    assert_eq_test!(cfg.wdog_ms, 100u32);
    assert_eq_test!(cfg.force_did, None);
    pass!()
}

/// Every knob parses from a fully populated command line.
pub fn xe_cmdline_all_knobs() -> TestResult {
    let cfg = cmdline::parse(
        "xe.diag=on xe.modeset=off xe.nocursor=on xe.pipe=B xe.wdog_ms=250 xe.force_did=0xA7A8",
    );
    assert_test!(cfg.diag, "xe.diag=on");
    assert_test!(
        !cfg.modeset,
        "xe.modeset=off keeps the firmware framebuffer"
    );
    assert_test!(cfg.nocursor, "xe.nocursor=on forces the software cursor");
    assert_eq_test!(cfg.pipe, Some(regs::Pipe::B));
    assert_eq_test!(cfg.wdog_ms, 250u32);
    assert_eq_test!(cfg.force_did, Some(0xa7a8u16));
    pass!()
}

/// Unknown tokens and malformed values are ignored (the driver still works on a
/// typo); `xe.modeset=off` is the recovery escape and `xe.force_did` accepts hex.
pub fn xe_cmdline_junk_and_overrides() -> TestResult {
    let cfg = cmdline::parse("foo bar=baz xe.unknown=1 xe.diag=maybe xe.modeset=off");
    assert_test!(!cfg.diag, "a malformed bool keeps the default");
    assert_test!(!cfg.modeset, "xe.modeset=off parsed");
    assert_test!(cfg.wdog_ms == 100, "untouched knobs keep their defaults");

    // Hex Device ID parses with either-case prefix and digits.
    assert_eq_test!(
        cmdline::parse("xe.force_did=0xA7A8").force_did,
        Some(0xa7a8u16)
    );
    assert_eq_test!(
        cmdline::parse("xe.force_did=0Xa7a8").force_did,
        Some(0xa7a8u16)
    );
    // A force_did without the 0x prefix is malformed and leaves the default.
    assert_eq_test!(cmdline::parse("xe.force_did=a7a8").force_did, None);
    // Each pipe selector parses case-insensitively.
    assert_eq_test!(cmdline::parse("xe.pipe=a").pipe, Some(regs::Pipe::A));
    assert_eq_test!(cmdline::parse("xe.pipe=C").pipe, Some(regs::Pipe::C));
    assert_eq_test!(cmdline::parse("xe.pipe=Z").pipe, None);
    pass!()
}

// =============================================================================
// cursor_config: M2 hardware-cursor plane encoding
// =============================================================================

/// `mode_for_side` selects the smallest ARGB cursor square that covers the
/// requested image, and rejects a zero or over-256 side.
pub fn xe_cursor_mode_for_side() -> TestResult {
    assert_eq_test!(
        cursor_config::mode_for_side(64),
        Some(cursor_config::CursorMode::Argb64)
    );
    assert_eq_test!(
        cursor_config::mode_for_side(128),
        Some(cursor_config::CursorMode::Argb128)
    );
    assert_eq_test!(
        cursor_config::mode_for_side(256),
        Some(cursor_config::CursorMode::Argb256)
    );
    // A size between the supported squares rounds up to the next one.
    assert_eq_test!(
        cursor_config::mode_for_side(100),
        Some(cursor_config::CursorMode::Argb128)
    );
    // Zero (no image) and any side past the 256x256 maximum have no mode.
    assert_test!(
        cursor_config::mode_for_side(0).is_none(),
        "a zero side has no cursor mode"
    );
    assert_test!(
        cursor_config::mode_for_side(257).is_none(),
        "a side past 256x256 has no cursor mode"
    );
    pass!()
}

/// `cur_ctl_value` emits the cross-checked i915 ARGB mode codes when enabled and
/// the bare disable code (whole mode field cleared) when not. On display IP
/// version 13 it also ORs in `MCURSOR_ARB_SLOTS(1)` (Wa_22012358565); earlier
/// versions get the bare mode code.
pub fn xe_cursor_ctl_value() -> TestResult {
    // Display IP version 12 carries no arbitration-slot workaround, so the value
    // is the bare i915 MCURSOR_MODE_{64,128,256}_ARGB_AX = 0x27 / 0x22 / 0x23.
    assert_eq_test!(
        cursor_config::cur_ctl_value(cursor_config::CursorMode::Argb64, true, 12),
        0x27u32
    );
    assert_eq_test!(
        cursor_config::cur_ctl_value(cursor_config::CursorMode::Argb128, true, 12),
        0x22u32
    );
    assert_eq_test!(
        cursor_config::cur_ctl_value(cursor_config::CursorMode::Argb256, true, 12),
        0x23u32
    );
    // Display IP version 13 (ADL-P / RPL-P) ORs in MCURSOR_ARB_SLOTS(1) = 1<<28
    // on top of the mode code — Wa_22012358565.
    let arb1 = regs::mcursor_arb_slots(1);
    assert_eq_test!(arb1, 0x1000_0000u32);
    assert_eq_test!(
        cursor_config::cur_ctl_value(cursor_config::CursorMode::Argb64, true, 13),
        0x27u32 | arb1
    );
    assert_eq_test!(
        cursor_config::cur_ctl_value(cursor_config::CursorMode::Argb256, true, 13),
        0x23u32 | arb1
    );
    // The arbitration-slot field lands in [30:28], never overlapping the mode bits.
    assert_eq_test!(
        regs::reg_field_get(
            regs::MCURSOR_ARB_SLOTS_MASK,
            cursor_config::cur_ctl_value(cursor_config::CursorMode::Argb256, true, 13)
        ),
        1u32
    );
    // The ARGB promote bit (bit 5) rides on every enabled mode, both versions.
    assert_test!(
        cursor_config::cur_ctl_value(cursor_config::CursorMode::Argb64, true, 13)
            & regs::MCURSOR_MODE_ARGB
            != 0,
        "an enabled cursor selects the 32-bpp ARGB format"
    );
    // Disabling clears the whole register regardless of size or version — the WA
    // bit is never set on a disabled cursor.
    assert_eq_test!(
        cursor_config::cur_ctl_value(cursor_config::CursorMode::Argb256, false, 13),
        regs::MCURSOR_MODE_DISABLE
    );
    assert_eq_test!(
        cursor_config::cur_ctl_value(cursor_config::CursorMode::Argb64, false, 13),
        0u32
    );
    pass!()
}

/// `cur_pos_pack` packs an on-screen position into the X/Y magnitude fields with
/// no sign bits, and an off-screen (negative) position with the sign bits set.
pub fn xe_cursor_pos_pack() -> TestResult {
    // Positive (10, 20): X in [14:0], Y in [30:16], neither sign bit set.
    let on_screen = cursor_config::cur_pos_pack(10, 20);
    assert_eq_test!(
        regs::reg_field_get(regs::CURSOR_POS_X_MASK, on_screen),
        10u32
    );
    assert_eq_test!(
        regs::reg_field_get(regs::CURSOR_POS_Y_MASK, on_screen),
        20u32
    );
    assert_test!(
        on_screen & regs::CURSOR_POS_X_SIGN == 0 && on_screen & regs::CURSOR_POS_Y_SIGN == 0,
        "an on-screen cursor sets no sign bit"
    );
    assert_eq_test!(on_screen, 0x0014_000au32);

    // Negative (-5, -3): magnitudes in the fields, both sign bits set (the
    // cursor hangs off the top-left corner).
    let off_screen = cursor_config::cur_pos_pack(-5, -3);
    assert_eq_test!(
        regs::reg_field_get(regs::CURSOR_POS_X_MASK, off_screen),
        5u32
    );
    assert_eq_test!(
        regs::reg_field_get(regs::CURSOR_POS_Y_MASK, off_screen),
        3u32
    );
    assert_test!(
        off_screen & regs::CURSOR_POS_X_SIGN != 0,
        "a negative X sets the X sign bit"
    );
    assert_test!(
        off_screen & regs::CURSOR_POS_Y_SIGN != 0,
        "a negative Y sets the Y sign bit"
    );
    pass!()
}

// =============================================================================
// regs: field helpers + per-pipe offsets
// =============================================================================

/// `bit`, `reg_field_get`, and `reg_field_set` agree as inverses on the
/// PLANE_CTL format and tiling fields.
pub fn xe_regs_field_helpers() -> TestResult {
    assert_eq_test!(regs::bit(0), 1u32);
    assert_eq_test!(regs::bit(12), 0x1000u32);
    assert_eq_test!(regs::bit(31), 0x8000_0000u32);

    // Format field [27:24]: place 4 (the 8:8:8:8 code) and read it back.
    let placed = regs::reg_field_set(regs::PLANE_CTL_FORMAT_MASK, 4);
    assert_eq_test!(placed, 0x0400_0000u32);
    assert_eq_test!(
        regs::reg_field_get(regs::PLANE_CTL_FORMAT_MASK, placed),
        4u32
    );

    // Tiling field [12:10]: place 4 (Y-tiled) and read it back.
    let tiling = regs::reg_field_set(regs::PLANE_CTL_TILING_MASK, 4);
    assert_eq_test!(tiling, 0x1000u32);
    assert_eq_test!(
        regs::reg_field_get(regs::PLANE_CTL_TILING_MASK, tiling),
        4u32
    );
    pass!()
}

/// Per-pipe register offsets follow the SKL+ 0x1000 stride from the pipe-A base.
pub fn xe_regs_per_pipe_offsets() -> TestResult {
    // Primary-plane control: pipe A 0x70180, B 0x71180, C 0x72180.
    assert_eq_test!(regs::plane_ctl(regs::Pipe::A), 0x70180usize);
    assert_eq_test!(regs::plane_ctl(regs::Pipe::B), 0x71180usize);
    assert_eq_test!(regs::plane_ctl(regs::Pipe::C), 0x72180usize);
    // Within-group offsets resolve off the same base.
    assert_eq_test!(regs::plane_surf(regs::Pipe::A), 0x7019cusize);
    assert_eq_test!(regs::plane_stride(regs::Pipe::A), 0x70188usize);
    // Pipe/transcoder and source-size registers.
    assert_eq_test!(regs::pipe_conf(regs::Pipe::A), 0x70008usize);
    assert_eq_test!(regs::pipe_conf(regs::Pipe::B), 0x71008usize);
    assert_eq_test!(regs::pipe_src(regs::Pipe::A), 0x6001cusize);
    assert_eq_test!(regs::pipe_src(regs::Pipe::B), 0x6101cusize);
    // Cursor plane base.
    assert_eq_test!(regs::cur_ctl(regs::Pipe::A), 0x70080usize);
    // The enum index and stride agree.
    assert_eq_test!(regs::Pipe::C.index(), 2usize);
    assert_eq_test!(regs::Pipe::B.stride_bytes(), 0x1000usize);
    assert_eq_test!(regs::Pipe::ALL.len(), 3usize);
    pass!()
}

// =============================================================================
// ddb: cursor DBUF/DDB allocation + watermark encoding (M2 support)
// =============================================================================

/// `decode_buf_cfg` / `encode_buf_cfg` round-trip, storing END as the inclusive
/// last block (end-exclusive minus one) like i915 `skl_ddb_entry_write`, and a
/// zero register decodes to / encodes from the empty range.
pub fn xe_ddb_buf_cfg_roundtrip() -> TestResult {
    // A 1080p-scale single-pipe primary allocation [0, 800).
    let primary = ddb::DdbEntry { start: 0, end: 800 };
    let reg = ddb::encode_buf_cfg(primary);
    // END field holds the inclusive last block (799), START holds 0.
    assert_eq_test!(regs::reg_field_get(regs::DDB_BUF_END_MASK, reg), 799u32);
    assert_eq_test!(regs::reg_field_get(regs::DDB_BUF_START_MASK, reg), 0u32);
    assert_eq_test!(ddb::decode_buf_cfg(reg), primary);

    // A non-zero start round-trips too.
    let offset = ddb::DdbEntry {
        start: 100,
        end: 800,
    };
    assert_eq_test!(ddb::decode_buf_cfg(ddb::encode_buf_cfg(offset)), offset);

    // The empty range is the zero register (no allocation), both ways.
    assert_eq_test!(
        ddb::encode_buf_cfg(ddb::DdbEntry { start: 0, end: 0 }),
        0u32
    );
    assert_eq_test!(ddb::decode_buf_cfg(0), ddb::DdbEntry { start: 0, end: 0 });
    assert_eq_test!(primary.blocks(), 800u32);
    pass!()
}

/// `carve_cursor_ddb` takes the cursor's blocks off the TAIL of the primary's
/// allocation, leaving disjoint, adjacent ranges, and refuses a primary too small
/// to keep more than it surrenders.
pub fn xe_ddb_carve_cursor() -> TestResult {
    let primary = ddb::DdbEntry { start: 0, end: 800 };
    let split = match ddb::carve_cursor_ddb(primary, ddb::CURSOR_DDB_BLOCKS) {
        Some(s) => s,
        None => return fail!("a large primary must yield a carve"),
    };
    // Cursor takes the tail; primary keeps the head; they are adjacent + disjoint.
    assert_eq_test!(split.cursor.blocks(), ddb::CURSOR_DDB_BLOCKS);
    assert_eq_test!(split.cursor.end, primary.end);
    assert_eq_test!(split.primary.start, primary.start);
    assert_eq_test!(split.primary.end, split.cursor.start);
    assert_eq_test!(
        split.primary.blocks() + split.cursor.blocks(),
        primary.blocks()
    );
    // Primary keeps strictly more than it surrenders.
    assert_test!(
        split.primary.blocks() > split.cursor.blocks(),
        "the shrunk primary still exceeds the cursor allocation"
    );

    // A primary with only 2x the cursor blocks (or fewer) is refused outright.
    assert_test!(
        ddb::carve_cursor_ddb(
            ddb::DdbEntry {
                start: 0,
                end: ddb::CURSOR_DDB_BLOCKS * 2
            },
            ddb::CURSOR_DDB_BLOCKS
        )
        .is_none(),
        "a primary that cannot keep more than it surrenders is refused"
    );
    assert_test!(
        ddb::carve_cursor_ddb(ddb::DdbEntry { start: 0, end: 0 }, ddb::CURSOR_DDB_BLOCKS).is_none(),
        "an empty primary allocation is refused"
    );
    pass!()
}

/// `wm_value` / `cursor_wm0` compose the enable, ignore-lines, lines, and blocks
/// fields; the cursor level-0 watermark is enabled, block-based, and strictly
/// below the cursor DDB allocation.
pub fn xe_ddb_wm_value() -> TestResult {
    let wm0 = ddb::cursor_wm0();
    assert_test!(wm0 & regs::WM_ENABLE != 0, "cursor WM0 is enabled");
    assert_test!(
        wm0 & regs::WM_IGNORE_LINES != 0,
        "cursor WM0 is block-based (ignore lines)"
    );
    assert_eq_test!(
        regs::reg_field_get(regs::WM_BLOCKS_MASK, wm0),
        ddb::CURSOR_WM0_BLOCKS
    );
    // The block count must stay strictly below the cursor's DDB allocation (i915
    // treats a watermark >= the plane's allocation as invalid).
    assert_test!(
        ddb::CURSOR_WM0_BLOCKS < ddb::CURSOR_DDB_BLOCKS,
        "the WM0 block count is below the cursor DDB allocation"
    );
    // Ignore-lines leaves the lines field clear.
    assert_eq_test!(regs::reg_field_get(regs::WM_LINES_MASK, wm0), 0u32);

    // A lines-based level places the lines field; a disabled level is all-zero.
    let lines_wm = ddb::wm_value(true, false, 4, 8);
    assert_eq_test!(regs::reg_field_get(regs::WM_LINES_MASK, lines_wm), 4u32);
    assert_eq_test!(regs::reg_field_get(regs::WM_BLOCKS_MASK, lines_wm), 8u32);
    assert_test!(lines_wm & regs::WM_IGNORE_LINES == 0, "lines mode set");
    assert_eq_test!(ddb::wm_value(false, true, 0, 8), 0u32);
    pass!()
}

slopos_testing::stest!(name = xe_platform_identifies_target_a7a8, suite = xe_logic);
slopos_testing::stest!(name = xe_platform_known_dids, suite = xe_logic);
slopos_testing::stest!(name = xe_platform_unknown_did_is_none, suite = xe_logic);
slopos_testing::stest!(name = xe_plane_size_pos_roundtrip, suite = xe_logic);
slopos_testing::stest!(name = xe_plane_decode_live_dspacntr, suite = xe_logic);
slopos_testing::stest!(name = xe_plane_encode_ctl_linear, suite = xe_logic);
slopos_testing::stest!(name = xe_plane_pixel_format_mapping, suite = xe_logic);
slopos_testing::stest!(name = xe_plane_linear_stride, suite = xe_logic);
slopos_testing::stest!(name = xe_ggtt_pte_encode, suite = xe_logic);
slopos_testing::stest!(name = xe_ggtt_offset_index_inverse, suite = xe_logic);
slopos_testing::stest!(name = xe_ggtt_region_overlaps, suite = xe_logic);
slopos_testing::stest!(name = xe_ggtt_alloc_above, suite = xe_logic);
slopos_testing::stest!(name = xe_cmdline_defaults, suite = xe_logic);
slopos_testing::stest!(name = xe_cmdline_all_knobs, suite = xe_logic);
slopos_testing::stest!(name = xe_cmdline_junk_and_overrides, suite = xe_logic);
slopos_testing::stest!(name = xe_cursor_mode_for_side, suite = xe_logic);
slopos_testing::stest!(name = xe_cursor_ctl_value, suite = xe_logic);
slopos_testing::stest!(name = xe_cursor_pos_pack, suite = xe_logic);
slopos_testing::stest!(name = xe_ddb_buf_cfg_roundtrip, suite = xe_logic);
slopos_testing::stest!(name = xe_ddb_carve_cursor, suite = xe_logic);
slopos_testing::stest!(name = xe_ddb_wm_value, suite = xe_logic);
slopos_testing::stest!(name = xe_regs_field_helpers, suite = xe_logic);
slopos_testing::stest!(name = xe_regs_per_pipe_offsets, suite = xe_logic);
