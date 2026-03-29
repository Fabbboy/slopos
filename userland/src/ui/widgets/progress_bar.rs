use slopos_abi::draw::Color32;

use crate::ui::constraints::{BoxConstraints, Rect, Size};
use crate::ui::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::ui::paint::PaintContext;
use crate::ui::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

pub struct ProgressBarWidget {
    id: WidgetId,
    rect: Rect,
    value: u32,
    label: String,
    color: Option<Color32>,
}

impl ProgressBarWidget {
    pub fn new(value: u32, label: String, color: Option<Color32>) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
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
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let height = ctx.style.line_height + 4;
        let size = Size::new(constraints.max_width, height);
        constraints.constrain(size)
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
    }

    fn paint(&self, ctx: &mut PaintContext) {
        // 1. Track background
        ctx.fill_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            ctx.style.bg_tertiary,
        );

        // 2. Fill portion
        let fill_w = self.rect.width * self.value as i32 / 100;
        if fill_w > 0 {
            ctx.fill_rect(
                self.rect.x,
                self.rect.y,
                fill_w,
                self.rect.height,
                self.fill_color(),
            );
        }

        // 3. Border outline
        ctx.draw_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            ctx.style.border_default,
        );

        // 4. Centered label text
        let tw = ctx.text_width(&self.label);
        let th = ctx.text_height();
        let tx = self.rect.x + (self.rect.width - tw) / 2;
        let ty = self.rect.y + (self.rect.height - th) / 2;
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

    fn id(&self) -> WidgetId {
        self.id
    }

    fn layout_rect(&self) -> Rect {
        self.rect
    }
}
