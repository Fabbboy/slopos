use std::any::Any;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventPhase, EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{
    FocusPolicy, MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget,
};

/// A child floated at an absolute position over its parent's area.
///
/// Occupies the parent's whole rect so a click outside the child still lands
/// here and can dismiss it.
pub struct PopupWidget {
    core: WidgetCore,
    anchor: (i32, i32),
    child: Box<dyn Widget>,
    on_dismiss: Option<Box<dyn Fn() -> Box<dyn Any>>>,
}

impl PopupWidget {
    pub fn new(
        x: i32,
        y: i32,
        child: Box<dyn Widget>,
        on_dismiss: Option<Box<dyn Fn() -> Box<dyn Any>>>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            anchor: (x, y),
            child,
            on_dismiss,
        }
    }

    fn dismiss(&self, sink: &mut MessageSink) -> EventResponse {
        if let Some(f) = &self.on_dismiss {
            sink.emit_raw(f());
        }
        EventResponse::Consumed
    }

    /// Position the child at the anchor, flipped/clamped to stay inside `rect`.
    /// Flip before clamp, so an edge-anchored menu does not cover the pointer.
    fn child_rect(&self) -> Rect {
        let rect = self.layout_rect();
        let size = self.child.measured_size();
        let (w, h) = (size.width, size.height);
        let (ax, ay) = self.anchor;

        let x = if ax + w > rect.x + rect.width && ax - w >= rect.x {
            ax - w
        } else {
            ax
        };
        let y = if ay + h > rect.y + rect.height && ay - h >= rect.y {
            ay - h
        } else {
            ay
        };

        let max_x = (rect.x + rect.width - w).max(rect.x);
        let max_y = (rect.y + rect.height - h).max(rect.y);
        Rect::new(x.clamp(rect.x, max_x), y.clamp(rect.y, max_y), w, h)
    }
}

impl Widget for PopupWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        measure_widget(self.child.as_mut(), constraints.loosen(), ctx);
        constraints.constrain(constraints.max_size())
    }

    fn layout(&mut self, _rect: Rect) {
        let child_rect = self.child_rect();
        place_widget(self.child.as_mut(), child_rect);
    }

    fn paint(&self, ctx: &mut PaintContext) {
        self.child.paint(ctx);
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        if phase != EventPhase::Target && phase != EventPhase::Bubble {
            return EventResponse::Ignored;
        }

        match event {
            WidgetEvent::KeyDown {
                key: Key::Named(NamedKey::Escape),
                ..
            } => self.dismiss(sink),

            WidgetEvent::PointerDown { x, y, .. } => {
                if !self.child.layout_rect().contains(*x, *y) {
                    return self.dismiss(sink);
                }
                self.child.event(event, EventPhase::Target, sink)
            }

            // Modal: swallow what the child ignores so the tree underneath
            // cannot act while the popup is open.
            _ => {
                let resp = self.child.event(event, phase, sink);
                if resp.is_consumed() {
                    resp
                } else {
                    EventResponse::Consumed
                }
            }
        }
    }

    fn role(&self) -> Role {
        Role::Group
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::StrongFocus
    }

    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}
