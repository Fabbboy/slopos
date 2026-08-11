use std::any::Any;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventPhase, EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

/// A child floated at an absolute position over its parent's area.
///
/// Occupies the parent's whole rect so a click anywhere outside the child is
/// still seen here and can dismiss; only the child's own rect paints.
pub struct PopupWidget {
    id: WidgetId,
    rect: Rect,
    anchor: (i32, i32),
    child: Box<dyn Widget>,
    child_size: Size,
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
            id: next_widget_id(),
            rect: Rect::ZERO,
            anchor: (x, y),
            child,
            child_size: Size::ZERO,
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
    ///
    /// Flipping before clamping is what keeps a menu opened near the right or
    /// bottom edge from covering the pointer that opened it.
    fn child_rect(&self) -> Rect {
        let (w, h) = (self.child_size.width, self.child_size.height);
        let (ax, ay) = self.anchor;

        let x = if ax + w > self.rect.x + self.rect.width && ax - w >= self.rect.x {
            ax - w
        } else {
            ax
        };
        let y = if ay + h > self.rect.y + self.rect.height && ay - h >= self.rect.y {
            ay - h
        } else {
            ay
        };

        let max_x = (self.rect.x + self.rect.width - w).max(self.rect.x);
        let max_y = (self.rect.y + self.rect.height - h).max(self.rect.y);
        Rect::new(
            x.clamp(self.rect.x, max_x),
            y.clamp(self.rect.y, max_y),
            w,
            h,
        )
    }
}

impl Widget for PopupWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.child_size = self.child.measure(constraints.loosen(), ctx);
        constraints.constrain(constraints.max_size())
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
        self.child.layout(self.child_rect());
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

            // A popup is modal over its parent: swallow whatever the child
            // ignores so the tree underneath cannot act while it is open.
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

    fn id(&self) -> WidgetId {
        self.id
    }

    fn layout_rect(&self) -> Rect {
        self.rect
    }

    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}
