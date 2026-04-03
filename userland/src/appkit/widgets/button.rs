use slopos_abi::draw::Color32;

use crate::appkit::constraints::{BoxConstraints, Rect, Size};
use crate::appkit::event::{
    EventPhase, EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent,
};
use crate::appkit::node::{ButtonStyle, MessageId};
use crate::appkit::paint::PaintContext;
use crate::appkit::style::StyleSheet;
use crate::appkit::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};
use crate::gfx::font;

/// Visual interaction state of the button.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ButtonState {
    Idle,
    Hovered,
    Pressed,
    Disabled,
}

/// Clickable button with a text label and visual states.
pub struct ButtonWidget {
    id: WidgetId,
    rect: Rect,
    label: String,
    on_press: Option<MessageId>,
    style: ButtonStyle,
    enabled: bool,
    state: ButtonState,
    focused: bool,
}

impl ButtonWidget {
    pub fn new(
        label: String,
        on_press: Option<MessageId>,
        style: ButtonStyle,
        enabled: bool,
    ) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            label,
            on_press,
            style,
            enabled,
            state: if enabled {
                ButtonState::Idle
            } else {
                ButtonState::Disabled
            },
            focused: false,
        }
    }
}

/// Lighten a color by adding to each RGB channel (clamped to 255).
fn lighten(c: Color32, amount: u8) -> Color32 {
    Color32::new(
        c.red().saturating_add(amount),
        c.green().saturating_add(amount),
        c.blue().saturating_add(amount),
        c.alpha(),
    )
}

/// Darken a color by subtracting from each RGB channel (clamped to 0).
fn darken(c: Color32, amount: u8) -> Color32 {
    Color32::new(
        c.red().saturating_sub(amount),
        c.green().saturating_sub(amount),
        c.blue().saturating_sub(amount),
        c.alpha(),
    )
}

/// Pick background and foreground colors for the given style + state.
fn button_colors(style: &StyleSheet, bs: ButtonStyle, state: ButtonState) -> (Color32, Color32) {
    match (bs, state) {
        (ButtonStyle::Primary, ButtonState::Idle) => (style.bg_accent, style.text_on_accent),
        (ButtonStyle::Primary, ButtonState::Hovered) => {
            (lighten(style.bg_accent, 20), style.text_on_accent)
        }
        (ButtonStyle::Primary, ButtonState::Pressed) => {
            (darken(style.bg_accent, 20), style.text_on_accent)
        }

        (ButtonStyle::Secondary, ButtonState::Idle) => (style.bg_secondary, style.text_primary),
        (ButtonStyle::Secondary, ButtonState::Hovered) => (style.bg_tertiary, style.text_primary),
        (ButtonStyle::Secondary, ButtonState::Pressed) => {
            (darken(style.bg_secondary, 10), style.text_primary)
        }

        (ButtonStyle::Destructive, ButtonState::Idle) => {
            (style.bg_destructive, style.text_on_accent)
        }
        (ButtonStyle::Destructive, ButtonState::Hovered) => {
            (lighten(style.bg_destructive, 20), style.text_on_accent)
        }
        (ButtonStyle::Destructive, ButtonState::Pressed) => {
            (darken(style.bg_destructive, 20), style.text_on_accent)
        }

        (_, ButtonState::Disabled) => (style.bg_secondary, style.text_disabled),
    }
}

impl Widget for ButtonWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let text_w = font::string_width(&self.label);
        let text_h = font::cell_height();
        let w = (text_w + ctx.style.button_padding_h * 2).max(ctx.style.button_min_width);
        let h = text_h + ctx.style.button_padding_v * 2;
        constraints.constrain(Size::new(w, h))
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let (bg, fg) = button_colors(ctx.style, self.style, self.state);

        // Filled rounded rectangle background.
        ctx.fill_rounded_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            ctx.style.corner_radius,
            bg,
        );

        // Center the label text both horizontally and vertically.
        let text_w = ctx.text_width(&self.label);
        let text_h = ctx.text_height();
        let tx = self.rect.x + (self.rect.width - text_w) / 2;
        let ty = self.rect.y + (self.rect.height - text_h) / 2;
        ctx.draw_text_transparent(tx, ty, &self.label, fg);

        // Focus ring when keyboard-focused.
        if self.focused {
            ctx.draw_focus_ring(self.rect);
        }
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        if phase != EventPhase::Target {
            return EventResponse::Ignored;
        }

        if !self.enabled {
            self.state = ButtonState::Disabled;
            return EventResponse::Ignored;
        }

        match event {
            WidgetEvent::PointerEnter => {
                if self.state == ButtonState::Idle {
                    self.state = ButtonState::Hovered;
                }
                EventResponse::Ignored
            }
            WidgetEvent::PointerLeave => {
                // Cancel press or exit hover.
                self.state = ButtonState::Idle;
                EventResponse::Ignored
            }
            WidgetEvent::PointerDown {
                button: PointerButton::Left,
                ..
            } => {
                // Transition to Pressed from any interactive state.
                // Don't require PointerEnter (hover) first — the framework
                // may not synthesize enter/leave events from pointer motion.
                if self.state != ButtonState::Disabled {
                    self.state = ButtonState::Pressed;
                }
                EventResponse::Consumed
            }
            WidgetEvent::PointerUp {
                button: PointerButton::Left,
                ..
            } => {
                if self.state == ButtonState::Pressed {
                    self.state = ButtonState::Hovered;
                    if let Some(msg) = self.on_press {
                        sink.emit(msg)
                    };
                    return EventResponse::Consumed;
                }
                EventResponse::Ignored
            }
            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                // If we were pressed when focus was lost, reset to idle.
                if self.state == ButtonState::Pressed {
                    self.state = ButtonState::Idle;
                }
                EventResponse::Ignored
            }
            WidgetEvent::KeyDown { key, .. } => {
                if matches!(
                    key,
                    Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space)
                ) {
                    if let Some(msg) = self.on_press {
                        sink.emit(msg)
                    };
                    return EventResponse::Consumed;
                }
                EventResponse::Ignored
            }
            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::Button
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
