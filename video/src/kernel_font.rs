//! Kernel font service — embeds Inter (proportional UI) and JetBrains Mono
//! (monospace console), initialises the global glyph atlas, and provides a
//! shared `FontRenderer` for proportional text.

use slopos_font::FontRenderer;
use slopos_font::atlas::GlyphAtlas;
use slopos_sync::IrqMutex;
use slopos_utils::{klog_info, klog_warn};

/// Embedded Inter Regular TTF (SIL Open Font License) — proportional UI font.
const INTER_TTF: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");

/// Embedded JetBrains Mono Regular TTF (SIL Open Font License) — monospace
/// console/terminal font.
const JETBRAINS_MONO_TTF: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");

/// Default font size (pixels) for the console glyph atlas.
const CONSOLE_FONT_SIZE: u16 = 16;

static FONT_RENDERER: IrqMutex<Option<FontRenderer<'static>>> = IrqMutex::new(None);

/// Initialise the kernel font subsystem.
///
/// Must be called after the heap allocator is available but before any
/// framebuffer text is rendered (splash screen, vconsole, etc.).
pub fn init() {
    // 1. Global fixed-width atlas using JetBrains Mono (monospace — every
    //    glyph has the same advance, so cells are naturally uniform).
    if slopos_font::atlas::init_global(JETBRAINS_MONO_TTF, CONSOLE_FONT_SIZE) {
        if let Some(atlas) = slopos_font::atlas::global() {
            klog_info!(
                "Font atlas ready: {}x{} cells (JetBrains Mono {}px)",
                atlas.cell_width(),
                atlas.cell_height(),
                CONSOLE_FONT_SIZE,
            );
        }
    } else {
        klog_warn!("Failed to initialise font atlas");
    }

    // 2. Proportional renderer using Inter (splash screen, roulette).
    if let Some(r) = FontRenderer::new(INTER_TTF) {
        *FONT_RENDERER.lock() = Some(r);
    }
}

/// Borrow the shared proportional `FontRenderer`.
///
/// The closure receives `&mut FontRenderer` so it can render text (which
/// mutates the internal glyph cache).  Returns `None` when the renderer
/// was never initialised.
pub fn with_renderer<R>(f: impl FnOnce(&mut FontRenderer<'static>) -> R) -> Option<R> {
    let mut guard = FONT_RENDERER.lock();
    guard.as_mut().map(f)
}

/// Get the global glyph atlas (convenience re-export).
#[inline]
pub fn atlas() -> Option<&'static GlyphAtlas> {
    slopos_font::atlas::global()
}
