use super::constraints::Rect;
use super::focus::{FocusManager, FocusScopeId};
use super::paint::PaintContext;
use super::traits::{Widget, WidgetId};

/// A popup, menu or tooltip layered above the main widget tree.
pub struct OverlayEntry {
    pub root: Box<dyn Widget>,
    /// Position in window coordinates.
    pub position: (i32, i32),
    /// Whether clicking outside dismisses this overlay.
    pub light_dismiss: bool,
    pub scope: Option<FocusScopeId>,
}

/// Manages a z-ordered stack of overlay layers (popups, menus).
pub struct OverlayManager {
    overlays: Vec<OverlayEntry>,
}

impl OverlayManager {
    pub fn new() -> Self {
        Self {
            overlays: Vec::new(),
        }
    }

    /// Push a new overlay; returns its index.
    pub fn push(
        &mut self,
        root: Box<dyn Widget>,
        position: (i32, i32),
        light_dismiss: bool,
        focus: &mut FocusManager,
    ) -> usize {
        let scope = if light_dismiss {
            let scope_id = focus.push_scope(Vec::new());
            Some(scope_id)
        } else {
            None
        };
        let idx = self.overlays.len();
        self.overlays.push(OverlayEntry {
            root,
            position,
            light_dismiss,
            scope,
        });
        idx
    }

    /// Pop the topmost overlay; returns whether one was removed.
    pub fn pop(&mut self, focus: &mut FocusManager) -> bool {
        if let Some(entry) = self.overlays.pop() {
            if entry.scope.is_some() {
                focus.pop_scope();
            }
            true
        } else {
            false
        }
    }

    pub fn dismiss_light(&mut self, focus: &mut FocusManager) {
        while self.overlays.last().map_or(false, |e| e.light_dismiss) {
            self.pop(focus);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.overlays.is_empty()
    }

    pub fn len(&self) -> usize {
        self.overlays.len()
    }

    /// Hit test in reverse z-order (topmost first); yields the overlay index.
    pub fn hit_test(&self, x: i32, y: i32) -> Option<(usize, WidgetId)> {
        for (i, entry) in self.overlays.iter().enumerate().rev() {
            let local_x = x - entry.position.0;
            let local_y = y - entry.position.1;
            if let Some(hit) = super::event::hit_test(entry.root.as_ref(), local_x, local_y) {
                return Some((i, hit.target));
            }
        }
        None
    }

    pub fn paint(&self, ctx: &mut PaintContext) {
        for entry in &self.overlays {
            let rect = entry.root.layout_rect();
            let overlay_rect =
                Rect::new(entry.position.0, entry.position.1, rect.width, rect.height);
            let root = &entry.root;
            ctx.with_clip(overlay_rect, |ctx| {
                root.paint(ctx);
            });
        }
    }

    pub fn overlays(&self) -> &[OverlayEntry] {
        &self.overlays
    }
}

impl Default for OverlayManager {
    fn default() -> Self {
        Self::new()
    }
}
