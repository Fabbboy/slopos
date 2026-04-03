use crate::appkit::constraints::{BoxConstraints, ImageScale, Rect, Size};
use crate::appkit::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::appkit::paint::PaintContext;
use crate::appkit::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

pub struct ImageWidget {
    id: WidgetId,
    rect: Rect,
    source_width: u32,
    source_height: u32,
    scale: ImageScale,
}

impl ImageWidget {
    pub fn new(width: u32, height: u32, scale: ImageScale) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            source_width: width,
            source_height: height,
            scale,
        }
    }
}

impl Widget for ImageWidget {
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let size = match self.scale {
            ImageScale::None => Size::new(self.source_width as i32, self.source_height as i32),
            ImageScale::Fit => {
                let sw = self.source_width as i32;
                let sh = self.source_height as i32;
                if sw == 0 || sh == 0 {
                    return constraints.constrain(Size::ZERO);
                }
                let max_w = constraints.max_width.min(i32::MAX / 2);
                let max_h = constraints.max_height.min(i32::MAX / 2);
                // Scale to fit preserving aspect ratio.
                let scale_w = max_w as f64 / sw as f64;
                let scale_h = max_h as f64 / sh as f64;
                let scale = scale_w.min(scale_h).min(1.0);
                Size::new((sw as f64 * scale) as i32, (sh as f64 * scale) as i32)
            }
            ImageScale::Fill => constraints.max_size(),
        };
        constraints.constrain(size)
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
    }

    fn paint(&self, ctx: &mut PaintContext) {
        // Placeholder: fill with bg_secondary. Actual pixel blitting would
        // require pixel data not stored in the Node for simplicity.
        ctx.fill_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            ctx.style.bg_secondary,
        );
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
