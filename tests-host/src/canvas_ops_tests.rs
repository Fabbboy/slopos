use slopos_abi::draw::{Canvas, Color32, EncodedPixel};
use slopos_abi::pixel::PixelFormat;

/// Minimal Canvas for testing. Stores pixels in a Vec<u8> as ARGB8888.
struct TestCanvas {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

impl TestCanvas {
    fn new(width: u32, height: u32) -> Self {
        Self {
            data: vec![0u8; (width * height * 4) as usize],
            width,
            height,
        }
    }

    fn read_pixel(&self, x: u32, y: u32) -> Color32 {
        let off = (y * self.width + x) as usize * 4;
        let raw = u32::from_le_bytes([
            self.data[off],
            self.data[off + 1],
            self.data[off + 2],
            self.data[off + 3],
        ]);
        PixelFormat::Argb8888.decode(raw)
    }
}

impl Canvas for TestCanvas {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn pitch_bytes(&self) -> usize {
        self.width as usize * 4
    }

    fn bytes_per_pixel(&self) -> u8 {
        4
    }

    fn pixel_format(&self) -> PixelFormat {
        PixelFormat::Argb8888
    }

    fn write_encoded_at(&mut self, byte_offset: usize, pixel: EncodedPixel) {
        let bytes = pixel.to_u32().to_le_bytes();
        if byte_offset + 4 <= self.data.len() {
            self.data[byte_offset..byte_offset + 4].copy_from_slice(&bytes);
        }
    }

    fn read_encoded_at(&self, byte_offset: usize) -> u32 {
        if byte_offset + 4 <= self.data.len() {
            u32::from_le_bytes([
                self.data[byte_offset],
                self.data[byte_offset + 1],
                self.data[byte_offset + 2],
                self.data[byte_offset + 3],
            ])
        } else {
            0
        }
    }
}

#[test]
fn line_draws_pixels() {
    let mut canvas = TestCanvas::new(10, 10);
    slopos_gfx::canvas_ops::line(&mut canvas, 0, 0, 9, 0, Color32::WHITE);
    // All pixels on row 0 should be white
    for x in 0..10 {
        let c = canvas.read_pixel(x, 0);
        assert_eq!(c, Color32::WHITE, "pixel ({x}, 0) not white");
    }
    // Row 1 should still be black/transparent
    let c = canvas.read_pixel(0, 1);
    assert_eq!(c.red(), 0);
}

#[test]
fn fill_rect_fills_area() {
    let mut canvas = TestCanvas::new(20, 20);
    let red = Color32::rgb(255, 0, 0);
    slopos_gfx::canvas_ops::fill_rect(&mut canvas, 5, 5, 3, 3, red);
    // Inside
    assert_eq!(canvas.read_pixel(5, 5), red);
    assert_eq!(canvas.read_pixel(7, 7), red);
    // Outside
    assert_ne!(canvas.read_pixel(4, 5), red);
    assert_ne!(canvas.read_pixel(8, 5), red);
}

#[test]
fn line_aa_draws_antialiased_pixels() {
    let mut canvas = TestCanvas::new(20, 20);
    let white = Color32::WHITE;
    let damage = slopos_gfx::canvas_ops::line_aa(&mut canvas, 0, 0, 19, 10, white);
    assert!(damage.is_some());
    // The line should have drawn something at the start
    let c = canvas.read_pixel(0, 0);
    assert!(c.red() > 0, "Start pixel should be non-zero");
}

#[test]
fn circle_aa_draws_antialiased_circle() {
    let mut canvas = TestCanvas::new(50, 50);
    let white = Color32::WHITE;
    let damage = slopos_gfx::canvas_ops::circle_aa(&mut canvas, 25, 25, 10, white);
    assert!(damage.is_some());

    // Check that pixels on the circle boundary are drawn
    // Top of circle at (25, 15) should have coverage
    let c = canvas.read_pixel(25, 15);
    assert!(c.red() > 0, "Top of circle should have coverage");

    // Center should be empty (it's an outline, not filled)
    let center = canvas.read_pixel(25, 25);
    assert_eq!(center.red(), 0, "Center should be empty for outline circle");
}

#[test]
fn rounded_rect_draws_corners_and_edges() {
    let mut canvas = TestCanvas::new(60, 40);
    let white = Color32::WHITE;
    let damage = slopos_gfx::canvas_ops::rounded_rect(&mut canvas, 5, 5, 50, 30, 8, white);
    assert!(damage.is_some());

    // Top edge (between corners) should be drawn
    let mid_top = canvas.read_pixel(30, 5);
    assert_eq!(mid_top, white, "Middle of top edge should be fully opaque");

    // A pixel in the interior should be empty (outline only)
    let interior = canvas.read_pixel(30, 20);
    assert_eq!(interior.red(), 0, "Interior should be empty");
}

#[test]
fn rounded_rect_filled_fills_interior() {
    let mut canvas = TestCanvas::new(60, 40);
    let color = Color32::rgb(100, 150, 200);
    let damage =
        slopos_gfx::canvas_ops::rounded_rect_filled(&mut canvas, 5, 5, 50, 30, 8, color);
    assert!(damage.is_some());

    // Interior should be filled
    let interior = canvas.read_pixel(30, 20);
    assert_eq!(interior, color, "Interior should be filled");

    // Outside should be empty
    let outside = canvas.read_pixel(0, 0);
    assert_eq!(outside.red(), 0, "Outside should be empty");
}

#[test]
fn rounded_rect_zero_radius_is_regular_rect() {
    let mut c1 = TestCanvas::new(30, 30);
    let mut c2 = TestCanvas::new(30, 30);
    let color = Color32::rgb(50, 100, 150);

    slopos_gfx::canvas_ops::rounded_rect(&mut c1, 5, 5, 20, 20, 0, color);
    slopos_gfx::canvas_ops::rect(&mut c2, 5, 5, 20, 20, color);

    assert_eq!(c1.data, c2.data, "Zero-radius rounded rect should equal regular rect");
}

#[test]
fn blend_put_pixel_blended_on_canvas() {
    let mut canvas = TestCanvas::new(10, 10);
    // First fill with blue
    let blue = Color32::rgb(0, 0, 255);
    slopos_gfx::canvas_ops::fill_rect(&mut canvas, 0, 0, 10, 10, blue);

    // Blend red at 50% alpha
    let semi_red = Color32::new(255, 0, 0, 128);
    slopos_gfx::blend::put_pixel_blended(&mut canvas, 5, 5, semi_red);

    let c = canvas.read_pixel(5, 5);
    // Should be a mix of red and blue
    assert!(c.red() > 50, "Should have red component, got {}", c.red());
    assert!(c.blue() > 50, "Should have blue component, got {}", c.blue());
    // Red shouldn't dominate completely
    assert!(c.red() < 200, "Red shouldn't be 255, got {}", c.red());
}

#[test]
fn fill_rect_blended_semitransparent() {
    let mut canvas = TestCanvas::new(10, 10);
    // Fill with white
    slopos_gfx::canvas_ops::fill_rect(&mut canvas, 0, 0, 10, 10, Color32::WHITE);

    // Blend black at 50% alpha over it
    let semi_black = Color32::new(0, 0, 0, 128);
    slopos_gfx::blend::fill_rect_blended(&mut canvas, 2, 2, 6, 6, semi_black);

    // Blended area should be ~gray
    let c = canvas.read_pixel(5, 5);
    let r = c.red() as i32;
    assert!(
        (r - 127).abs() <= 5,
        "Expected ~127 gray, got {r}"
    );

    // Outside blended area should still be white
    let outside = canvas.read_pixel(0, 0);
    assert_eq!(outside, Color32::WHITE);
}
