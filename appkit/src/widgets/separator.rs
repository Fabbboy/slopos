use crate::constraints::{BoxConstraints, Orientation, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

pub struct SeparatorWidget {
    core: WidgetCore,
    orientation: Orientation,
}

impl SeparatorWidget {
    pub fn new() -> Self {
        Self {
            core: WidgetCore::new(),
            orientation: Orientation::Horizontal,
        }
    }

    pub fn with_orientation(mut self, orientation: Orientation) -> Self {
        self.orientation = orientation;
        self
    }
}

impl Widget for SeparatorWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        // Orientation follows the axis the parent left unbounded: a VStack frees
        // height, so the divider runs across the width.
        let h_free = !constraints.is_height_bounded();
        let w_free = !constraints.is_width_bounded();

        if h_free && !w_free {
            self.orientation = Orientation::Horizontal;
            constraints.constrain(Size::new(constraints.max_width, 1))
        } else if w_free && !h_free {
            self.orientation = Orientation::Vertical;
            constraints.constrain(Size::new(1, constraints.max_height))
        } else {
            constraints.constrain(Size::new(1, 1))
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let color = ctx.style.border_divider;
        ctx.fill_rect(rect.x, rect.y, rect.width, rect.height, color);
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
}
