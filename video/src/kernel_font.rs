//! Kernel font service — dual-phase font initialization.
//!
//! **Phase 1 (early boot):** Embedded TTF fonts compiled into the kernel
//! binary via `include_bytes!`. Always available — used for panic screens,
//! splash, vconsole, and roulette animations.
//!
//! **Phase 2 (post-VFS):** Stub for future filesystem-based font loading.
//! Currently a no-op because kernel fonts use `&'static [u8]` lifetimes
//! and the font data must outlive the kernel (same pattern as Linux's
//! compiled-in `lib/fonts/` bitmap fonts).

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

/// Phase 1: Initialise the kernel font subsystem from embedded fonts.
///
/// Must be called after the heap allocator is available but before any
/// framebuffer text is rendered (splash screen, vconsole, etc.).
///
/// Uses compiled-in TTF data — guaranteed to succeed regardless of
/// filesystem state. This is the same pattern as Linux's built-in
/// bitmap console fonts (`lib/fonts/font_8x16.c`).
pub fn init() {
    // 1. Global fixed-width atlas using JetBrains Mono (monospace — every
    //    glyph has the same advance, so cells are naturally uniform).
    if slopos_font::atlas::init_global(JETBRAINS_MONO_TTF, CONSOLE_FONT_SIZE) {
        if let Some(atlas) = slopos_font::atlas::global() {
            klog_info!(
                "Font atlas ready: {}x{} cells (JetBrains Mono {}px, source={:?})",
                atlas.cell_width(),
                atlas.cell_height(),
                CONSOLE_FONT_SIZE,
                atlas.source(),
            );
        }
    } else {
        klog_warn!("Failed to initialise font atlas");
    }

    // 2. Proportional renderer using Inter (splash screen, roulette).
    if let Some(r) = FontRenderer::new(INTER_TTF) {
        klog_info!("Proportional font renderer ready (Inter, source={:?})", r.source());
        *FONT_RENDERER.lock() = Some(r);
    }
}

/// Phase 2: Attempt to upgrade fonts from the filesystem (post-VFS).
///
/// Called after the VFS is mounted. Currently a no-op because:
/// 1. Kernel fonts must be `&'static [u8]` — filesystem data would need
///    `Box::leak()` which is fine but adds complexity.
/// 2. The panic screen must use embedded fonts (filesystem may not be
///    available during a panic).
/// 3. For an OS kernel, compiled-in fonts are the gold standard (Linux
///    never loads console fonts from the filesystem in-kernel).
///
/// This hook exists for future use if runtime font swapping is desired
/// (e.g., via a `SYS_FONT_SET` syscall similar to Linux's `KDFONTOP`).
pub fn try_upgrade_from_filesystem() {
    // Intentional no-op. See doc comment above.
    // Future: could reload the proportional renderer from /usr/share/fonts/
    // and rebuild the console atlas with a filesystem-loaded TTF.
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
