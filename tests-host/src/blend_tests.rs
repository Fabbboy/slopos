use slopos_gfx::blend::{alpha_blend, blend_coverage};
use slopos_abi::draw::Color32;

#[test]
fn blend_fully_opaque_src_returns_src() {
    let src = Color32::rgb(200, 100, 50).to_u32();
    let dst = Color32::rgb(10, 20, 30).to_u32();
    assert_eq!(alpha_blend(src, dst), src);
}

#[test]
fn blend_fully_transparent_src_returns_dst() {
    let src = Color32::new(200, 100, 50, 0).to_u32();
    let dst = Color32::rgb(10, 20, 30).to_u32();
    assert_eq!(alpha_blend(src, dst), dst);
}

#[test]
fn blend_50_percent_alpha_on_opaque_dst() {
    // src: red=200, alpha=128 (≈50%) over dst: red=100, alpha=255
    let src = Color32::new(200, 0, 0, 128).to_u32();
    let dst = Color32::rgb(100, 0, 0).to_u32();
    let result = alpha_blend(src, dst);
    let result_color = Color32(result);

    // Expected: out_a = 128 + 255*(255-128)/255 = 128 + 127 = 255
    // out_r = (200*128 + 100*255*127/255) / 255 = (25600 + 12700) / 255 ≈ 150
    assert_eq!(result_color.alpha(), 255);
    // Allow ±2 rounding tolerance
    let r = result_color.red() as i32;
    assert!(
        (r - 150).abs() <= 2,
        "Expected red ~150, got {r}"
    );
}

#[test]
fn blend_regression_old_bug_too_bright() {
    // The old buggy formula (treating src channels as premultiplied) would
    // produce: r = 200 + (100*127+127)/255 = 200 + 50 = 250
    // The correct result should be ~150, NOT 250.
    let src = Color32::new(200, 0, 0, 128).to_u32();
    let dst = Color32::rgb(100, 0, 0).to_u32();
    let result = alpha_blend(src, dst);
    let r = Color32(result).red();
    assert!(
        r < 200,
        "Blend result red={r} is way too bright — old premultiplied bug likely present"
    );
}

#[test]
fn blend_25_percent_alpha_channels() {
    // src: (R=255, G=0, B=0, A=64) over dst: (R=0, G=0, B=255, A=255)
    let src = Color32::new(255, 0, 0, 64).to_u32();
    let dst = Color32::rgb(0, 0, 255).to_u32();
    let result = alpha_blend(src, dst);
    let c = Color32(result);

    // out_a ≈ 255
    assert_eq!(c.alpha(), 255);
    // out_r = (255*64 + 0*255*191/255) / 255 ≈ 64
    let r = c.red() as i32;
    assert!((r - 64).abs() <= 2, "Expected red ~64, got {r}");
    // out_b = (0*64 + 255*255*191/255) / 255 ≈ 191
    let b = c.blue() as i32;
    assert!((b - 191).abs() <= 2, "Expected blue ~191, got {b}");
}

#[test]
fn blend_both_semitransparent() {
    // src alpha=128 over dst alpha=128
    let src = Color32::new(255, 0, 0, 128).to_u32();
    let dst = Color32::new(0, 0, 255, 128).to_u32();
    let result = alpha_blend(src, dst);
    let c = Color32(result);

    // out_a = 128 + 128*(255-128)/255 ≈ 128 + 64 = 192
    let a = c.alpha() as i32;
    assert!((a - 192).abs() <= 2, "Expected alpha ~192, got {a}");
}

#[test]
fn blend_coverage_zero_returns_dst() {
    let fg = Color32::rgb(255, 0, 0);
    let dst = Color32::rgb(0, 0, 255).to_u32();
    assert_eq!(blend_coverage(0, fg, dst), dst);
}

#[test]
fn blend_coverage_full_opaque_fg_returns_fg() {
    let fg = Color32::rgb(255, 0, 0);
    let dst = Color32::rgb(0, 0, 255).to_u32();
    let result = blend_coverage(255, fg, dst);
    assert_eq!(result, fg.to_u32());
}

#[test]
fn blend_coverage_half_coverage() {
    let fg = Color32::rgb(200, 0, 0);
    let dst = Color32::rgb(0, 0, 100).to_u32();
    let result = blend_coverage(128, fg, dst);
    let c = Color32(result);

    // effective_alpha = 255 * 128 / 255 = 128
    // out_r = (200*128 + 0*(255-128) + 127) / 255 ≈ 100
    let r = c.red() as i32;
    assert!(
        (r - 100).abs() <= 3,
        "Expected red ~100, got {r}"
    );
    // out_b should be roughly half of 100
    let b = c.blue() as i32;
    assert!(
        (b - 50).abs() <= 3,
        "Expected blue ~50, got {b}"
    );
}

#[test]
fn blend_black_on_white_stays_reasonable() {
    // Black with 50% alpha on white should give ~128 gray
    let src = Color32::new(0, 0, 0, 128).to_u32();
    let dst = Color32::WHITE.to_u32();
    let result = alpha_blend(src, dst);
    let c = Color32(result);
    let r = c.red() as i32;
    let g = c.green() as i32;
    let b = c.blue() as i32;
    assert!(
        (r - 127).abs() <= 3 && (g - 127).abs() <= 3 && (b - 127).abs() <= 3,
        "Expected ~(127,127,127), got ({r},{g},{b})"
    );
}

#[test]
fn blend_white_on_black_stays_reasonable() {
    // White with 50% alpha on black should give ~128 gray
    let src = Color32::new(255, 255, 255, 128).to_u32();
    let dst = Color32::BLACK.to_u32();
    let result = alpha_blend(src, dst);
    let c = Color32(result);
    let r = c.red() as i32;
    assert!(
        (r - 128).abs() <= 3,
        "Expected ~128, got {r}"
    );
}
