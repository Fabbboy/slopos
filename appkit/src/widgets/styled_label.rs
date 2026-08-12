use slopos_abi::draw::Color32;

use crate::constraints::{BoxConstraints, Size, TextAlignment};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

/// Label with an explicit foreground color that overrides the theme.
pub struct StyledLabelWidget {
    core: WidgetCore,
    text: String,
    color: Color32,
    alignment: TextAlignment,
}

impl StyledLabelWidget {
    pub fn new(text: String, color: Color32, alignment: TextAlignment) -> Self {
        Self {
            core: WidgetCore::new(),
            text,
            color,
            alignment,
        }
    }
}

impl Widget for StyledLabelWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let text_w = crate::text::string_width(&self.text);
        let line_height = ctx.style.line_height;
        constraints.constrain(Size::new(text_w, line_height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let tw = ctx.text_width(&self.text);
        let x = match self.alignment {
            TextAlignment::Start => rect.x,
            TextAlignment::Center => rect.x + (rect.width - tw) / 2,
            TextAlignment::End => rect.x + rect.width - tw,
        };
        ctx.draw_text_transparent(x, rect.y, &self.text, self.color);
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
}
