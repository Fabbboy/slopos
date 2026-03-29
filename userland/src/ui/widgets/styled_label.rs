use slopos_abi::draw::Color32;

use crate::ui::constraints::{BoxConstraints, Rect, Size, TextAlignment};
use crate::ui::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::ui::paint::PaintContext;
use crate::ui::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

/// Label with an explicit foreground color that overrides the theme.
pub struct StyledLabelWidget {
    id: WidgetId,
    rect: Rect,
    text: String,
    color: Color32,
    alignment: TextAlignment,
}

impl StyledLabelWidget {
    pub fn new(text: String, color: Color32, alignment: TextAlignment) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            text,
            color,
            alignment,
        }
    }
}

impl Widget for StyledLabelWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let text_w = crate::gfx::font::string_width(&self.text);
        let line_height = ctx.style.line_height;
        constraints.constrain(Size::new(text_w, line_height))
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let tw = ctx.text_width(&self.text);
        let x = match self.alignment {
            TextAlignment::Start => self.rect.x,
            TextAlignment::Center => self.rect.x + (self.rect.width - tw) / 2,
            TextAlignment::End => self.rect.x + self.rect.width - tw,
        };
        ctx.draw_text_transparent(x, self.rect.y, &self.text, self.color);
    }

    fn event(
        &mut self,
        _event: &WidgetEvent,
        _phase: EventPhase,
        _sink: &mut MessageSink,
    ) -> EventResponse {
        EventResponse::Ignored
    }

    fn role(&self) -> Role {
        Role::Label
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.text)
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }

    fn id(&self) -> WidgetId {
        self.id
    }

    fn layout_rect(&self) -> Rect {
        self.rect
    }
}
