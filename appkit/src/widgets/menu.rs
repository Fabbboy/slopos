use std::any::Any;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventPhase, EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::node::{MenuItem, MenuItemKind};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

/// Popup menu rendered as a regular widget.
///
/// Overlay positioning is handled externally by the OverlayManager;
/// this widget just renders the menu items and handles navigation.
pub struct MenuWidget {
    id: WidgetId,
    rect: Rect,
    items: Vec<MenuItem>,
    on_action: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    hovered_index: Option<usize>,
    focused: bool,
    /// Item height from the last measure. Hit testing must use the same value
    /// the paint pass used, or clicks land on the row above or below.
    item_height: i32,
}

impl MenuWidget {
    pub fn new(
        items: Vec<MenuItem>,
        on_action: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    ) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            items,
            on_action,
            hovered_index: None,
            focused: false,
            item_height: 0,
        }
    }

    /// Index of the item at window-space `y`, or `None` outside the menu.
    fn item_at_y(&self, y: i32) -> Option<usize> {
        if self.item_height <= 0 {
            return None;
        }
        let rel_y = y - self.rect.y;
        if rel_y < 0 || rel_y >= self.rect.height {
            return None;
        }
        let idx = (rel_y / self.item_height) as usize;
        (idx < self.items.len()).then_some(idx)
    }

    fn is_activatable(&self, idx: usize) -> bool {
        self.items
            .get(idx)
            .is_some_and(|i| matches!(i.kind, MenuItemKind::Action) && i.enabled)
    }

    /// Find the next actionable (non-separator, enabled) item index, wrapping around.
    fn next_actionable(&self, from: Option<usize>, forward: bool) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        let len = self.items.len();
        let start = match from {
            Some(idx) => {
                if forward {
                    (idx + 1) % len
                } else {
                    (idx + len - 1) % len
                }
            }
            None => {
                if forward {
                    0
                } else {
                    len - 1
                }
            }
        };

        for offset in 0..len {
            let idx = if forward {
                (start + offset) % len
            } else {
                (start + len - offset) % len
            };
            if self.is_activatable(idx) {
                return Some(idx);
            }
        }
        None
    }
}

impl Widget for MenuWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let item_h = ctx.style.menu_item_height;
        self.item_height = item_h;
        let padding_h = ctx.style.spacing_md;

        // Width = max(label_width + padding + shortcut_width, menu_min_width).
        let mut max_label_w = 0i32;
        let mut max_shortcut_w = 0i32;

        for item in &self.items {
            match &item.kind {
                MenuItemKind::Action | MenuItemKind::Submenu(_) => {
                    let lw = crate::text::string_width(item.label);
                    max_label_w = max_label_w.max(lw);
                    if let Some(sc) = item.shortcut {
                        let sw = crate::text::string_width(sc);
                        max_shortcut_w = max_shortcut_w.max(sw);
                    }
                }
                MenuItemKind::Separator => {}
            }
        }

        let shortcut_gap = if max_shortcut_w > 0 {
            ctx.style.spacing_lg
        } else {
            0
        };
        let w = (padding_h * 2 + max_label_w + shortcut_gap + max_shortcut_w)
            .max(ctx.style.menu_min_width);
        let h = self.items.len() as i32 * item_h;

        constraints.constrain(Size::new(w, h))
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let item_h = self.item_height;
        let padding_h = ctx.style.spacing_md;
        let radius = ctx.style.corner_radius;

        // Background with rounded corners and border.
        ctx.fill_rounded_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            radius,
            ctx.style.bg_primary,
        );
        ctx.draw_rounded_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            radius,
            ctx.style.border_default,
        );

        let text_h = ctx.text_height();

        for (i, item) in self.items.iter().enumerate() {
            let y = self.rect.y + i as i32 * item_h;

            match &item.kind {
                MenuItemKind::Separator => {
                    // 1px horizontal line centered in the item row.
                    let line_y = y + item_h / 2;
                    ctx.fill_rect(
                        self.rect.x + padding_h,
                        line_y,
                        self.rect.width - padding_h * 2,
                        1,
                        ctx.style.border_divider,
                    );
                }
                MenuItemKind::Action | MenuItemKind::Submenu(_) => {
                    // Hover highlight.
                    if self.hovered_index == Some(i) && item.enabled {
                        ctx.fill_rect(
                            self.rect.x + 1,
                            y,
                            self.rect.width - 2,
                            item_h,
                            ctx.style.bg_accent,
                        );
                    }

                    // Label.
                    let fg = if !item.enabled {
                        ctx.style.text_disabled
                    } else if self.hovered_index == Some(i) {
                        ctx.style.text_on_accent
                    } else {
                        ctx.style.text_primary
                    };

                    let label_x = self.rect.x + padding_h;
                    let label_y = y + (item_h - text_h) / 2;
                    ctx.draw_text_transparent(label_x, label_y, item.label, fg);

                    // Shortcut (right-aligned).
                    if let Some(sc) = item.shortcut {
                        let sc_w = ctx.text_width(sc);
                        let sc_x = self.rect.x + self.rect.width - padding_h - sc_w;
                        let sc_fg = if self.hovered_index == Some(i) && item.enabled {
                            ctx.style.text_on_accent
                        } else {
                            ctx.style.text_secondary
                        };
                        ctx.draw_text_transparent(sc_x, label_y, sc, sc_fg);
                    }
                }
            }
        }
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        if phase != EventPhase::Target && phase != EventPhase::Bubble {
            return EventResponse::Ignored;
        }

        match event {
            WidgetEvent::PointerMove { y, .. } => {
                let hovered = self.item_at_y(*y).filter(|&idx| self.is_activatable(idx));
                let changed = hovered != self.hovered_index;
                self.hovered_index = hovered;
                if changed {
                    EventResponse::Consumed
                } else {
                    EventResponse::Ignored
                }
            }

            WidgetEvent::PointerDown { y, .. } => {
                let Some(idx) = self.item_at_y(*y).filter(|&i| self.is_activatable(i)) else {
                    return EventResponse::Ignored;
                };
                if let Some(cb) = &self.on_action {
                    sink.emit_raw(cb(idx));
                }
                EventResponse::Consumed
            }

            WidgetEvent::KeyDown { key, .. } => match key {
                Key::Named(NamedKey::Up) => {
                    self.hovered_index = self.next_actionable(self.hovered_index, false);
                    EventResponse::Consumed
                }
                Key::Named(NamedKey::Down) => {
                    self.hovered_index = self.next_actionable(self.hovered_index, true);
                    EventResponse::Consumed
                }
                Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space) => {
                    let Some(idx) = self.hovered_index.filter(|&i| self.is_activatable(i)) else {
                        return EventResponse::Ignored;
                    };
                    if let Some(cb) = &self.on_action {
                        sink.emit_raw(cb(idx));
                    }
                    EventResponse::Consumed
                }
                // Left as Ignored so the enclosing Popup sees it and dismisses;
                // consuming it here would trap the menu open.
                Key::Named(NamedKey::Escape) => EventResponse::Ignored,
                _ => EventResponse::Ignored,
            },

            WidgetEvent::FocusGained => {
                self.focused = true;
                // Auto-select first actionable item on focus.
                if self.hovered_index.is_none() {
                    self.hovered_index = self.next_actionable(None, true);
                }
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                self.hovered_index = None;
                EventResponse::Ignored
            }

            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::Menu
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::StrongFocus
    }

    fn id(&self) -> WidgetId {
        self.id
    }

    fn layout_rect(&self) -> Rect {
        self.rect
    }
}
