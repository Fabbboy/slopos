use slopos_abi::draw::Color32;
use slopos_abi::pixel::PixelFormat;

#[test]
fn encode_decode_argb8888_roundtrip() {
    let colors = [
        Color32::BLACK,
        Color32::WHITE,
        Color32::TRANSPARENT,
        Color32::new(255, 0, 0, 255),   // red
        Color32::new(0, 255, 0, 128),   // green, half alpha
        Color32::new(0, 0, 255, 1),     // blue, almost transparent
        Color32::new(123, 45, 67, 200), // arbitrary
    ];

    for &color in &colors {
        let fmt = PixelFormat::Argb8888;
        let encoded = fmt.encode(color);
        let decoded = fmt.decode(encoded.to_u32());
        assert_eq!(
            decoded, color,
            "Argb8888 roundtrip failed for 0x{:08X}",
            color.to_u32()
        );
    }
}

#[test]
fn encode_decode_xrgb8888_strips_alpha() {
    let fmt = PixelFormat::Xrgb8888;
    let color = Color32::new(100, 150, 200, 50);
    let encoded = fmt.encode(color);
    let decoded = fmt.decode(encoded.to_u32());
    // Xrgb always decodes alpha as 0xFF
    assert_eq!(decoded.alpha(), 0xFF);
    assert_eq!(decoded.red(), 100);
    assert_eq!(decoded.green(), 150);
    assert_eq!(decoded.blue(), 200);
}

#[test]
fn encode_decode_rgba8888_roundtrip() {
    let fmt = PixelFormat::Rgba8888;
    let color = Color32::new(10, 20, 30, 200);
    let encoded = fmt.encode(color);
    let decoded = fmt.decode(encoded.to_u32());
    assert_eq!(decoded, color);
}

#[test]
fn encode_decode_bgra8888_roundtrip() {
    let fmt = PixelFormat::Bgra8888;
    let color = Color32::new(10, 20, 30, 200);
    let encoded = fmt.encode(color);
    let decoded = fmt.decode(encoded.to_u32());
    assert_eq!(decoded, color);
}

#[test]
fn encode_decode_rgb888_roundtrip() {
    let fmt = PixelFormat::Rgb888;
    let color = Color32::rgb(100, 150, 200);
    let encoded = fmt.encode(color);
    let decoded = fmt.decode(encoded.to_u32());
    assert_eq!(decoded.red(), 100);
    assert_eq!(decoded.green(), 150);
    assert_eq!(decoded.blue(), 200);
    assert_eq!(decoded.alpha(), 0xFF);
}

#[test]
fn encode_decode_bgr888_roundtrip() {
    let fmt = PixelFormat::Bgr888;
    let color = Color32::rgb(100, 150, 200);
    let encoded = fmt.encode(color);
    let decoded = fmt.decode(encoded.to_u32());
    assert_eq!(decoded.red(), 100);
    assert_eq!(decoded.green(), 150);
    assert_eq!(decoded.blue(), 200);
    assert_eq!(decoded.alpha(), 0xFF);
}

#[test]
fn decode_is_inverse_of_encode_all_formats() {
    let formats = [
        PixelFormat::Argb8888,
        PixelFormat::Rgba8888,
        PixelFormat::Bgra8888,
        PixelFormat::Rgb888,
        PixelFormat::Bgr888,
    ];
    // Test with a variety of channel values
    for r in [0u8, 1, 127, 128, 254, 255] {
        for g in [0u8, 100, 255] {
            for b in [0u8, 50, 255] {
                let color = Color32::rgb(r, g, b);
                for &fmt in &formats {
                    let encoded = fmt.encode(color);
                    let decoded = fmt.decode(encoded.to_u32());
                    assert_eq!(decoded.red(), r, "fmt={fmt:?} r mismatch");
                    assert_eq!(decoded.green(), g, "fmt={fmt:?} g mismatch");
                    assert_eq!(decoded.blue(), b, "fmt={fmt:?} b mismatch");
                }
            }
        }
    }
}
