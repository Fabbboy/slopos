use std::any::Any;

use crate::constraints::{BoxConstraints, Rect, ScrollDirection, ScrollbarVisibility, Size};
use crate::event::{EventPhase, EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

/// Scrollable container with a single child widget.
pub struct ScrollViewWidget {
    id: WidgetId,
    rect: Rect,
    child: Box<dyn Widget>,
    direction: ScrollDirection,
    show_scrollbar: ScrollbarVisibility,
    offset_x: i32,
    offset_y: i32,
    content_size: Size,
    viewport_size: Size,
    thumb_dragging: bool,
    thumb_hovered: bool,
    drag_start_y: i32,
    drag_start_offset: i32,
    focused: bool,
    on_scroll: Option<Box<dyn Fn(i32) -> Box<dyn Any>>>,
}

impl ScrollViewWidget {
    pub fn new(
        child: Box<dyn Widget>,
        direction: ScrollDirection,
        show_scrollbar: ScrollbarVisibility,
    ) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            child,
            direction,
            show_scrollbar,
            offset_x: 0,
            offset_y: 0,
            content_size: Size::ZERO,
            viewport_size: Size::ZERO,
            thumb_dragging: false,
            thumb_hovered: false,
            drag_start_y: 0,
            drag_start_offset: 0,
            focused: false,
            on_scroll: None,
        }
    }

    pub fn with_scroll(
        child: Box<dyn Widget>,
        direction: ScrollDirection,
        show_scrollbar: ScrollbarVisibility,
        scroll_y: i32,
        on_scroll: Option<Box<dyn Fn(i32) -> Box<dyn Any>>>,
    ) -> Self {
        let mut sv = Self::new(child, direction, show_scrollbar);
        sv.offset_y = scroll_y;
        sv.on_scroll = on_scroll;
        sv
    }

    fn max_offset_x(&self) -> i32 {
        (self.content_size.width - self.viewport_size.width).max(0)
    }

    fn max_offset_y(&self) -> i32 {
        (self.content_size.height - self.viewport_size.height).max(0)
    }

    fn clamp_offsets(&mut self) {
        self.offset_x = self.offset_x.clamp(0, self.max_offset_x());
        self.offset_y = self.offset_y.clamp(0, self.max_offset_y());
    }

    fn needs_vertical_scrollbar(&self) -> bool {
        match self.show_scrollbar {
            ScrollbarVisibility::Always => {
                matches!(
                    self.direction,
                    ScrollDirection::Vertical | ScrollDirection::Both
                )
            }
            ScrollbarVisibility::WhenNeeded => {
                matches!(
                    self.direction,
                    ScrollDirection::Vertical | ScrollDirection::Both
                ) && self.content_size.height > self.viewport_size.height
            }
            ScrollbarVisibility::Never => false,
        }
    }

    fn needs_horizontal_scrollbar(&self) -> bool {
        match self.show_scrollbar {
            ScrollbarVisibility::Always => {
                matches!(
                    self.direction,
                    ScrollDirection::Horizontal | ScrollDirection::Both
                )
            }
            ScrollbarVisibility::WhenNeeded => {
                matches!(
                    self.direction,
                    ScrollDirection::Horizontal | ScrollDirection::Both
                ) && self.content_size.width > self.viewport_size.width
            }
            ScrollbarVisibility::Never => false,
        }
    }

    /// Returns (track_rect, thumb_rect) for the vertical scrollbar.
    fn vertical_scrollbar_rects(&self, sb_width: i32, thumb_min: i32) -> (Rect, Rect) {
        let track = Rect::new(
            self.rect.x + self.rect.width - sb_width,
            self.rect.y,
            sb_width,
            self.rect.height,
        );

        let track_len = track.height;
        let max_off = self.max_offset_y();
        let thumb_size = if max_off > 0 && self.content_size.height > 0 {
            ((self.viewport_size.height as i64 * track_len as i64)
                / self.content_size.height as i64) as i32
        } else {
            track_len
        }
        .max(thumb_min)
        .min(track_len);

        let thumb_pos = if max_off > 0 {
            ((self.offset_y as i64 * (track_len - thumb_size) as i64) / max_off as i64) as i32
        } else {
            0
        };

        let thumb = Rect::new(track.x, track.y + thumb_pos, sb_width, thumb_size);

        (track, thumb)
    }

    /// Check if a point (in window coords) is within the vertical scrollbar thumb.
    fn point_in_v_thumb(&self, px: i32, py: i32, sb_width: i32, thumb_min: i32) -> bool {
        let (_, thumb) = self.vertical_scrollbar_rects(sb_width, thumb_min);
        thumb.contains(px, py)
    }
}

impl Widget for ScrollViewWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        // Measure child with unbounded constraints on the scroll axis.
        let child_constraints = match self.direction {
            ScrollDirection::Vertical => BoxConstraints {
                min_width: constraints.min_width,
                max_width: constraints.max_width,
                min_height: 0,
                max_height: i32::MAX,
            },
            ScrollDirection::Horizontal => BoxConstraints {
                min_width: 0,
                max_width: i32::MAX,
                min_height: constraints.min_height,
                max_height: constraints.max_height,
            },
            ScrollDirection::Both => BoxConstraints::UNBOUNDED,
        };

        self.content_size = self.child.measure(child_constraints, ctx);

        // Own size fills available space from parent.
        constraints.constrain(Size::new(constraints.max_width, constraints.max_height))
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
        self.viewport_size = Size::new(rect.width, rect.height);

        // Lay out child at offset position with its measured size.
        self.child.layout(Rect::new(
            rect.x - self.offset_x,
            rect.y - self.offset_y,
            self.content_size.width,
            self.content_size.height,
        ));

        self.clamp_offsets();
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let viewport = self.rect;

        // Clip child painting to the viewport.
        ctx.with_clip(viewport, |ctx| {
            self.child.paint(ctx);
        });

        let sb_width = ctx.style.scrollbar_width;
        let thumb_min = ctx.style.scrollbar_thumb_min;

        // Paint vertical scrollbar.
        if self.needs_vertical_scrollbar() {
            let (track, thumb) = self.vertical_scrollbar_rects(sb_width, thumb_min);

            // Track background.
            ctx.fill_rect(
                track.x,
                track.y,
                track.width,
                track.height,
                ctx.style.bg_secondary,
            );

            // Thumb.
            let thumb_color = if self.thumb_hovered || self.thumb_dragging {
                ctx.style.border_default
            } else {
                ctx.style.bg_tertiary
            };
            ctx.fill_rect(thumb.x, thumb.y, thumb.width, thumb.height, thumb_color);
        }

        // Paint horizontal scrollbar (similar pattern).
        if self.needs_horizontal_scrollbar() {
            let track_x = self.rect.x;
            let track_y = self.rect.y + self.rect.height - sb_width;
            let track_w = self.rect.width;

            ctx.fill_rect(track_x, track_y, track_w, sb_width, ctx.style.bg_secondary);

            let max_off = self.max_offset_x();
            let thumb_size = if max_off > 0 && self.content_size.width > 0 {
                ((self.viewport_size.width as i64 * track_w as i64)
                    / self.content_size.width as i64) as i32
            } else {
                track_w
            }
            .max(thumb_min)
            .min(track_w);

            let thumb_pos = if max_off > 0 {
                ((self.offset_x as i64 * (track_w - thumb_size) as i64) / max_off as i64) as i32
            } else {
                0
            };

            ctx.fill_rect(
                track_x + thumb_pos,
                track_y,
                thumb_size,
                sb_width,
                ctx.style.bg_tertiary,
            );
        }

        // Focus ring.
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
        if phase != EventPhase::Target && phase != EventPhase::Bubble {
            return EventResponse::Ignored;
        }

        let line_height = 20; // Default line height for scroll calculations.

        match event {
            WidgetEvent::Scroll { delta_x, delta_y } => {
                // delta_x/delta_y are already in pixels (converted by input.rs from v120).
                let can_scroll_v = matches!(
                    self.direction,
                    ScrollDirection::Vertical | ScrollDirection::Both
                );
                let can_scroll_h = matches!(
                    self.direction,
                    ScrollDirection::Horizontal | ScrollDirection::Both
                );

                let old_x = self.offset_x;
                let old_y = self.offset_y;

                if can_scroll_v && *delta_y != 0 {
                    self.offset_y = (self.offset_y + *delta_y).clamp(0, self.max_offset_y());
                }
                if can_scroll_h && *delta_x != 0 {
                    self.offset_x = (self.offset_x + *delta_x).clamp(0, self.max_offset_x());
                }

                if self.offset_x != old_x || self.offset_y != old_y {
                    self.child.layout(Rect::new(
                        self.rect.x - self.offset_x,
                        self.rect.y - self.offset_y,
                        self.content_size.width,
                        self.content_size.height,
                    ));
                    if let Some(cb) = &self.on_scroll {
                        sink.emit_raw(cb(self.offset_y));
                    }
                    EventResponse::Consumed
                } else {
                    EventResponse::Ignored
                }
            }

            WidgetEvent::PointerDown { x, y, .. } => {
                // Check if click is on the scrollbar thumb.
                if self.needs_vertical_scrollbar() {
                    let sb_width = 8;
                    let thumb_min = 20;
                    if self.point_in_v_thumb(*x, *y, sb_width, thumb_min) {
                        self.thumb_dragging = true;
                        self.drag_start_y = *y;
                        self.drag_start_offset = self.offset_y;
                        return EventResponse::CapturePointer;
                    }
                }
                EventResponse::Ignored
            }

            WidgetEvent::PointerMove { x, y } => {
                if self.thumb_dragging {
                    let sb_width = 8;
                    let thumb_min = 20;
                    let (track, _) = self.vertical_scrollbar_rects(sb_width, thumb_min);
                    let track_len = track.height;

                    let thumb_size = if self.max_offset_y() > 0 && self.content_size.height > 0 {
                        ((self.viewport_size.height as i64 * track_len as i64)
                            / self.content_size.height as i64) as i32
                    } else {
                        track_len
                    }
                    .max(thumb_min)
                    .min(track_len);

                    let usable = track_len - thumb_size;
                    if usable > 0 {
                        let dy = *y - self.drag_start_y;
                        let new_offset = self.drag_start_offset
                            + ((dy as i64 * self.max_offset_y() as i64) / usable as i64) as i32;
                        self.offset_y = new_offset.clamp(0, self.max_offset_y());

                        self.child.layout(Rect::new(
                            self.rect.x - self.offset_x,
                            self.rect.y - self.offset_y,
                            self.content_size.width,
                            self.content_size.height,
                        ));
                    }
                    return EventResponse::Consumed;
                }

                // Update thumb hover state.
                if self.needs_vertical_scrollbar() {
                    let sb_width = 8;
                    let thumb_min = 20;
                    self.thumb_hovered = self.point_in_v_thumb(*x, *y, sb_width, thumb_min);
                }
                EventResponse::Ignored
            }

            WidgetEvent::PointerUp { .. } => {
                if self.thumb_dragging {
                    self.thumb_dragging = false;
                    return EventResponse::ReleasePointer;
                }
                EventResponse::Ignored
            }

            WidgetEvent::KeyDown { key, .. } => {
                let can_v = matches!(
                    self.direction,
                    ScrollDirection::Vertical | ScrollDirection::Both
                );
                let can_h = matches!(
                    self.direction,
                    ScrollDirection::Horizontal | ScrollDirection::Both
                );

                let old_x = self.offset_x;
                let old_y = self.offset_y;

                match key {
                    Key::Named(NamedKey::Up) if can_v => {
                        self.offset_y = (self.offset_y - line_height).clamp(0, self.max_offset_y());
                    }
                    Key::Named(NamedKey::Down) if can_v => {
                        self.offset_y = (self.offset_y + line_height).clamp(0, self.max_offset_y());
                    }
                    Key::Named(NamedKey::Left) if can_h => {
                        self.offset_x = (self.offset_x - line_height).clamp(0, self.max_offset_x());
                    }
                    Key::Named(NamedKey::Right) if can_h => {
                        self.offset_x = (self.offset_x + line_height).clamp(0, self.max_offset_x());
                    }
                    Key::Named(NamedKey::PageUp) if can_v => {
                        self.offset_y = (self.offset_y - self.viewport_size.height)
                            .clamp(0, self.max_offset_y());
                    }
                    Key::Named(NamedKey::PageDown) if can_v => {
                        self.offset_y = (self.offset_y + self.viewport_size.height)
                            .clamp(0, self.max_offset_y());
                    }
                    Key::Named(NamedKey::Home) if can_v => {
                        self.offset_y = 0;
                    }
                    Key::Named(NamedKey::End) if can_v => {
                        self.offset_y = self.max_offset_y();
                    }
                    _ => return EventResponse::Ignored,
                }

                if self.offset_x != old_x || self.offset_y != old_y {
                    self.child.layout(Rect::new(
                        self.rect.x - self.offset_x,
                        self.rect.y - self.offset_y,
                        self.content_size.width,
                        self.content_size.height,
                    ));
                    EventResponse::Consumed
                } else {
                    EventResponse::Ignored
                }
            }

            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                EventResponse::Ignored
            }

            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::ScrollArea
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::TabFocus
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
