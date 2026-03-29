use slopos_abi::draw::Color32;

use crate::ui::constraints::{BoxConstraints, Rect, Size};
use crate::ui::event::{
    EventPhase, EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent,
};
use crate::ui::node::MessageId;
use crate::ui::paint::PaintContext;
use crate::ui::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

pub struct CheckboxWidget {
    id: WidgetId,
    rect: Rect,
    checked: bool,
    label: String,
    on_toggle: MessageId,
    enabled: bool,
    hovered: bool,
}

impl CheckboxWidget {
    pub fn new(checked: bool, label: String, on_toggle: MessageId, enabled: bool) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            checked,
            label,
            on_toggle,
            enabled,
            hovered: false,
        }
    }

    fn toggle(&mut self) {
        if self.enabled {
            self.checked = !self.checked;
        }
    }
}

impl Widget for CheckboxWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let cb_size = ctx.style.checkbox_size;
        let gap = ctx.style.checkbox_gap;
        // Approximate text width: 8px per char (bitmap font cell width).
        let text_w = self.label.len() as i32 * 8;
        let text_h = 16; // bitmap font cell height

        let width = cb_size + gap + text_w;
        let height = cb_size.max(text_h);
        constraints.constrain(Size::new(width, height))
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let style = ctx.style;
        let cb_size = style.checkbox_size;
        let gap = style.checkbox_gap;
        let text_h = 16_i32;

        // Vertically center the checkbox box within the rect.
        let box_y = self.rect.y + (self.rect.height - cb_size) / 2;
        let box_x = self.rect.x;

        if self.checked {
            // Filled accent background.
            ctx.fill_rounded_rect(
                box_x,
                box_y,
                cb_size,
                cb_size,
                style.corner_radius.min(cb_size / 4),
                if self.enabled {
                    style.bg_accent
                } else {
                    style.text_disabled
                },
            );
            // Draw checkmark as two lines using small filled rects.
            // Checkmark path scaled to cb_size: (3,8)->(7,12) and (7,12)->(13,4)
            // For a 16x16 box, draw "V" using pixel strips.
            draw_checkmark(ctx, box_x, box_y, cb_size, Color32::WHITE);
        } else {
            // Outline only.
            let border_color = if self.enabled {
                style.border_default
            } else {
                style.text_disabled
            };
            ctx.draw_rounded_rect(
                box_x,
                box_y,
                cb_size,
                cb_size,
                style.corner_radius.min(cb_size / 4),
                border_color,
            );
        }

        // Draw label text to the right.
        let text_x = self.rect.x + cb_size + gap;
        let text_y = self.rect.y + (self.rect.height - text_h) / 2;
        let fg = if self.enabled {
            style.text_primary
        } else {
            style.text_disabled
        };
        ctx.draw_text_transparent(text_x, text_y, &self.label, fg);

        // Focus ring around the checkbox box (not the label).
        let box_rect = Rect::new(box_x, box_y, cb_size, cb_size);
        ctx.draw_focus_ring(box_rect);
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        if phase != EventPhase::Target || !self.enabled {
            return EventResponse::Ignored;
        }
        match event {
            WidgetEvent::PointerDown {
                button: PointerButton::Left,
                ..
            } => {
                self.toggle();
                sink.emit(self.on_toggle);
                EventResponse::Consumed
            }
            WidgetEvent::PointerEnter => {
                self.hovered = true;
                EventResponse::Ignored
            }
            WidgetEvent::PointerLeave => {
                self.hovered = false;
                EventResponse::Ignored
            }
            WidgetEvent::KeyDown {
                key: Key::Named(NamedKey::Space),
                ..
            } => {
                self.toggle();
                sink.emit(self.on_toggle);
                EventResponse::Consumed
            }
            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::Checkbox
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.label)
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

/// Draw a simple checkmark inside a checkbox box using filled rectangles.
/// Path: short leg (3,8)->(7,12), long leg (7,12)->(13,4), scaled to `size`.
fn draw_checkmark(ctx: &mut PaintContext, bx: i32, by: i32, size: i32, color: Color32) {
    // Scale factors relative to a 16x16 reference.
    let sx = |v: i32| -> i32 { bx + v * size / 16 };
    let sy = |v: i32| -> i32 { by + v * size / 16 };
    let t = (size / 8).max(1); // stroke thickness

    // Short leg: (3,8) -> (7,12) — 4 steps diagonal down-right
    for i in 0..5 {
        ctx.fill_rect(sx(3 + i), sy(8 + i), t, t, color);
    }
    // Long leg: (7,12) -> (13,4) — 6 steps diagonal up-right
    for i in 0..7 {
        ctx.fill_rect(sx(7 + i), sy(12 - i), t, t, color);
    }
}
