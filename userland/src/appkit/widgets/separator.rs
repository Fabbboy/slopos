use crate::appkit::constraints::{BoxConstraints, Orientation, Rect, Size};
use crate::appkit::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::appkit::paint::PaintContext;
use crate::appkit::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

pub struct SeparatorWidget {
    id: WidgetId,
    rect: Rect,
    orientation: Orientation,
}

impl SeparatorWidget {
    pub fn new() -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            orientation: Orientation::Horizontal,
        }
    }

    pub fn with_orientation(mut self, orientation: Orientation) -> Self {
        self.orientation = orientation;
        self
    }
}

impl Widget for SeparatorWidget {
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        // Auto-detect orientation from parent layout context:
        // - VStack child: max_height is unbounded (i32::MAX), max_width is bounded → HORIZONTAL
        // - HStack child: max_width is unbounded (i32::MAX), max_height is bounded → VERTICAL
        let h_unbounded = constraints.max_height >= i32::MAX / 2;
        let w_unbounded = constraints.max_width >= i32::MAX / 2;

        if h_unbounded && !w_unbounded {
            // In a VStack (height unbounded) → horizontal line (fill width, 1px tall)
            self.orientation = Orientation::Horizontal;
            constraints.constrain(Size::new(constraints.max_width, 1))
        } else if w_unbounded && !h_unbounded {
            // In a HStack (width unbounded) → vertical line (1px wide, fill height)
            self.orientation = Orientation::Vertical;
            constraints.constrain(Size::new(1, constraints.max_height))
        } else {
            // Both bounded or both unbounded → 1x1, parent stretches
            constraints.constrain(Size::new(1, 1))
        }
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let color = ctx.style.border_divider;
        ctx.fill_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            color,
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
        Role::Separator
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
