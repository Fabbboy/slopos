use slopos_abi::draw::Color32;

use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

pub struct ProgressBarWidget {
    core: WidgetCore,
    value: u32,
    label: String,
    color: Option<Color32>,
}

impl ProgressBarWidget {
    pub fn new(value: u32, label: String, color: Option<Color32>) -> Self {
        Self {
            core: WidgetCore::new(),
            value: value.min(100),
            label,
            color,
        }
    }

    pub fn set_value(&mut self, value: u32) {
        self.value = value.min(100);
    }

    pub fn set_label(&mut self, label: String) {
        self.label = label;
    }

    fn fill_color(&self) -> Color32 {
        if let Some(c) = self.color {
            return c;
        }
        if self.value < 50 {
            Color32::rgb(46, 170, 78)
        } else if self.value < 80 {
            Color32::rgb(204, 170, 34)
        } else {
            Color32::rgb(204, 51, 51)
        }
    }
}

impl Widget for ProgressBarWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let height = ctx.style.line_height + 4;
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            ctx.style
                .field_min_width
                .max(crate::text::string_width(&self.label))
        };
        constraints.constrain(Size::new(width, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();

        ctx.fill_rect(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            ctx.style.bg_tertiary,
        );

        let fill_w = rect.width * self.value as i32 / 100;
        if fill_w > 0 {
            ctx.fill_rect(rect.x, rect.y, fill_w, rect.height, self.fill_color());
        }

        ctx.draw_rect(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            ctx.style.border_default,
        );

        let tw = ctx.text_width(&self.label);
        let th = ctx.text_height();
        let tx = rect.x + (rect.width - tw) / 2;
        let ty = rect.y + (rect.height - th) / 2;
        ctx.draw_text_transparent(tx, ty, &self.label, ctx.style.text_primary);
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
        Role::None
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }
}
