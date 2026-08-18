use std::any::Any;

use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventPhase, EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::node::{MenuItem, MenuItemKind};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

/// Renders items and handles navigation; positioning is the enclosing
/// `PopupWidget`'s job.
pub struct MenuWidget {
    core: WidgetCore,
    items: Vec<MenuItem>,
    on_action: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    hovered_index: Option<usize>,
    focused: bool,
    /// Item height from the last measure; hit testing must use the same value
    /// paint did, or clicks land on the neighbouring row.
    item_height: i32,
}

impl MenuWidget {
    pub fn new(
        items: Vec<MenuItem>,
        on_action: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
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
        let rect = self.layout_rect();
        let rel_y = y - rect.y;
        if rel_y < 0 || rel_y >= rect.height {
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

    /// Next non-separator, enabled item, wrapping around.
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
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let item_h = ctx.style.menu_item_height;
        self.item_height = item_h;
        let padding_h = ctx.style.spacing_md;

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

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let item_h = self.item_height;
        let padding_h = ctx.style.spacing_md;
        let radius = ctx.style.corner_radius;

        ctx.fill_rounded_rect(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            radius,
            ctx.style.bg_primary,
        );
        ctx.draw_rounded_rect(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            radius,
            ctx.style.border_default,
        );

        let text_h = ctx.text_height();

        for (i, item) in self.items.iter().enumerate() {
            let y = rect.y + i as i32 * item_h;

            match &item.kind {
                MenuItemKind::Separator => {
                    let line_y = y + item_h / 2;
                    ctx.fill_rect(
                        rect.x + padding_h,
                        line_y,
                        rect.width - padding_h * 2,
                        1,
                        ctx.style.border_divider,
                    );
                }
                MenuItemKind::Action | MenuItemKind::Submenu(_) => {
                    if self.hovered_index == Some(i) && item.enabled {
                        ctx.fill_rect(rect.x + 1, y, rect.width - 2, item_h, ctx.style.bg_accent);
                    }

                    let fg = if !item.enabled {
                        ctx.style.text_disabled
                    } else if self.hovered_index == Some(i) {
                        ctx.style.text_on_accent
                    } else {
                        ctx.style.text_primary
                    };

                    let label_x = rect.x + padding_h;
                    let label_y = y + (item_h - text_h) / 2;
                    ctx.draw_text_transparent(label_x, label_y, item.label, fg);

                    if let Some(sc) = item.shortcut {
                        let sc_w = ctx.text_width(sc);
                        let sc_x = rect.x + rect.width - padding_h - sc_w;
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

            WidgetEvent::PointerDown { x, y, .. } => {
                if !self.layout_rect().contains(*x, *y) {
                    return EventResponse::Ignored;
                }
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
                // Ignored so the enclosing Popup sees Escape and dismisses.
                Key::Named(NamedKey::Escape) => EventResponse::Ignored,
                _ => EventResponse::Ignored,
            },

            WidgetEvent::FocusGained => {
                self.focused = true;
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
}
