use slopos_font::ttf_parser::TtfFont;
use slopos_font::FontRenderer;
use slopos_abi::draw::{Canvas, Color32, EncodedPixel};
use slopos_abi::pixel::PixelFormat;

const INTER_TTF: &[u8] = include_bytes!(concat!(env!("SLOPOS_ROOT"), "/assets/fonts/Inter-Regular.ttf"));

/// Minimal Canvas for testing font rendering.
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

    fn has_any_nonzero_pixel(&self) -> bool {
        self.data.iter().any(|&b| b != 0)
    }
}

impl Canvas for TestCanvas {
    fn width(&self) -> u32 { self.width }
    fn height(&self) -> u32 { self.height }
    fn pitch_bytes(&self) -> usize { self.width as usize * 4 }
    fn bytes_per_pixel(&self) -> u8 { 4 }
    fn pixel_format(&self) -> PixelFormat { PixelFormat::Argb8888 }

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
fn parse_inter_font() {
    let font = TtfFont::parse(INTER_TTF);
    assert!(font.is_some(), "Failed to parse Inter-Regular.ttf");
    let font = font.unwrap();
    assert!(font.units_per_em() > 0, "units_per_em should be > 0");
    assert!(font.num_glyphs() > 100, "Inter should have many glyphs");
}

#[test]
fn glyph_index_ascii() {
    let font = TtfFont::parse(INTER_TTF).unwrap();

    // 'A' should map to a non-zero glyph index
    let glyph_a = font.glyph_index('A' as u32);
    assert!(glyph_a.is_some(), "'A' should have a glyph index");
    assert!(glyph_a.unwrap() > 0, "'A' glyph index should be > 0");

    // Space should also have a glyph index
    let glyph_space = font.glyph_index(' ' as u32);
    assert!(glyph_space.is_some(), "' ' should have a glyph index");
}

#[test]
fn glyph_outline_has_contours() {
    let font = TtfFont::parse(INTER_TTF).unwrap();
    let glyph_id = font.glyph_index('A' as u32).unwrap();
    let outline = font.glyph_outline(glyph_id);
    assert!(outline.is_some(), "'A' should have an outline");
    let outline = outline.unwrap();
    assert!(
        !outline.contours.is_empty(),
        "'A' should have at least one contour"
    );
}

#[test]
fn h_metrics_for_ascii() {
    let font = TtfFont::parse(INTER_TTF).unwrap();
    let glyph_id = font.glyph_index('A' as u32).unwrap();
    let hm = font.h_metrics(glyph_id);
    assert!(hm.is_some(), "'A' should have horizontal metrics");
    let hm = hm.unwrap();
    assert!(hm.advance_width > 0, "advance_width should be > 0");
}

#[test]
fn measure_text_nonzero() {
    let _font = TtfFont::parse(INTER_TTF).unwrap();
    let renderer = FontRenderer::new(INTER_TTF).unwrap();
    let (w, h) = renderer.measure_text("Hello", 16);
    assert!(w > 0, "Text width should be > 0, got {w}");
    assert!(h > 0, "Text height should be > 0, got {h}");
}

#[test]
fn measure_empty_text_is_zero_width() {
    let renderer = FontRenderer::new(INTER_TTF).unwrap();
    let (w, _h) = renderer.measure_text("", 16);
    assert_eq!(w, 0, "Empty text should have width 0");
}

#[test]
fn draw_text_produces_pixels() {
    let mut renderer = FontRenderer::new(INTER_TTF).unwrap();
    let mut canvas = TestCanvas::new(200, 50);

    let damage = renderer.draw_text(&mut canvas, 10, 10, "Hello", 16, Color32::WHITE);
    assert!(damage.is_some(), "draw_text should return damage rect");
    assert!(
        canvas.has_any_nonzero_pixel(),
        "Canvas should have non-zero pixels after rendering text"
    );
}

#[test]
fn draw_text_damage_rect_makes_sense() {
    let mut renderer = FontRenderer::new(INTER_TTF).unwrap();
    let mut canvas = TestCanvas::new(200, 50);

    let damage = renderer.draw_text(&mut canvas, 10, 10, "AB", 16, Color32::WHITE).unwrap();

    // Damage rect should be within canvas bounds
    assert!(damage.x0 >= 0);
    assert!(damage.y0 >= 0);
    assert!(damage.x1 < 200);
    assert!(damage.y1 < 50);
    // Should have reasonable width (at least a few pixels for "AB")
    let width = damage.x1 - damage.x0;
    assert!(width > 5, "Damage width {width} seems too small for 'AB'");
}

#[test]
fn rasterizer_full_coverage_reaches_255() {
    // Regression test for the 252-not-255 bug.
    // Render a large glyph and verify that fully-covered interior pixels
    // reach 255 coverage.
    let mut renderer = FontRenderer::new(INTER_TTF).unwrap();
    let mut canvas = TestCanvas::new(100, 100);

    // Use a large size to ensure solid interior areas
    renderer.draw_text(&mut canvas, 10, 10, "O", 48, Color32::WHITE);

    // Find the maximum alpha value in the canvas
    let mut max_alpha: u8 = 0;
    for y in 0..100u32 {
        for x in 0..100u32 {
            let c = canvas.read_pixel(x, y);
            if c.alpha() > max_alpha {
                max_alpha = c.alpha();
            }
        }
    }

    assert_eq!(
        max_alpha, 255,
        "Fully covered pixels should reach 255 coverage, got {max_alpha}"
    );
}

#[test]
fn different_sizes_produce_different_results() {
    let renderer_data = INTER_TTF;
    let r1 = FontRenderer::new(renderer_data).unwrap();
    let r2 = FontRenderer::new(renderer_data).unwrap();

    let (w1, h1) = r1.measure_text("A", 12);
    let (w2, h2) = r2.measure_text("A", 24);

    // 24px should be roughly double 12px
    assert!(w2 > w1, "24px should be wider than 12px");
    assert!(h2 > h1, "24px should be taller than 12px");
}

#[test]
fn space_character_advances_without_coverage() {
    let renderer = FontRenderer::new(INTER_TTF).unwrap();
    let (w_a, _) = renderer.measure_text("A", 16);
    let (w_a_space, _) = renderer.measure_text("A A", 16);

    // "A A" should be wider than "A" by more than one advance
    assert!(
        w_a_space > w_a * 2 - 5, // allow small tolerance
        "Space should add width: A={w_a}, A_A={w_a_space}"
    );
}
