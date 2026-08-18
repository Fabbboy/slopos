use slopos_abi::draw::Color32;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent,
};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

pub struct CheckboxWidget {
    core: WidgetCore,
    checked: bool,
    label: String,
    on_toggle: Option<Box<dyn Fn() -> Box<dyn std::any::Any>>>,
    enabled: bool,
    hovered: bool,
}

impl CheckboxWidget {
    pub fn new(
        checked: bool,
        label: String,
        on_toggle: Option<Box<dyn Fn() -> Box<dyn std::any::Any>>>,
        enabled: bool,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
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
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let cb_size = ctx.style.checkbox_size;
        let gap = ctx.style.checkbox_gap;
        let text_w = crate::text::string_width(&self.label);
        let text_h = crate::text::cell_height();

        let width = cb_size + gap + text_w;
        let height = cb_size.max(text_h);
        constraints.constrain(Size::new(width, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let style = ctx.style;
        let cb_size = style.checkbox_size;
        let gap = style.checkbox_gap;
        let text_h = ctx.text_height();

        let box_y = rect.y + (rect.height - cb_size) / 2;
        let box_x = rect.x;

        if self.checked {
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
            draw_checkmark(ctx, box_x, box_y, cb_size, Color32::WHITE);
        } else {
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

        let text_x = rect.x + cb_size + gap;
        let text_y = rect.y + (rect.height - text_h) / 2;
        let fg = if self.enabled {
            style.text_primary
        } else {
            style.text_disabled
        };
        ctx.draw_text_transparent(text_x, text_y, &self.label, fg);

        // Ring the box, not the label.
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
                x,
                y,
                button: PointerButton::Left,
            } => {
                if !self.layout_rect().contains(*x, *y) {
                    return EventResponse::Ignored;
                }
                self.toggle();
                if let Some(f) = &self.on_toggle {
                    sink.emit_raw(f());
                }
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
                if let Some(f) = &self.on_toggle {
                    sink.emit_raw(f());
                }
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
}

/// Checkmark path in 16x16 reference space, scaled to `size`: short leg
/// (3,8)->(7,12), long leg (7,12)->(13,4).
fn draw_checkmark(ctx: &mut PaintContext, bx: i32, by: i32, size: i32, color: Color32) {
    let sx = |v: i32| -> i32 { bx + v * size / 16 };
    let sy = |v: i32| -> i32 { by + v * size / 16 };
    let t = (size / 8).max(1); // stroke thickness

    for i in 0..5 {
        ctx.fill_rect(sx(3 + i), sy(8 + i), t, t, color);
    }
    for i in 0..7 {
        ctx.fill_rect(sx(7 + i), sy(12 - i), t, t, color);
    }
}
