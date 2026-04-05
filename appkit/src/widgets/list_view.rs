use std::any::Any;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventPhase, EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

/// Virtualized list with fixed item height.
///
/// Only items in the visible range are painted, keeping performance
/// constant regardless of total item count.
pub struct ListViewWidget {
    id: WidgetId,
    rect: Rect,

    item_height: i32,
    selected: Option<usize>,
    on_select: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    items: Vec<Box<dyn Widget>>,
    scroll_offset: i32,
    focused: bool,
}

impl ListViewWidget {
    pub fn new(
        item_height: i32,
        selected: Option<usize>,
        on_select: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
        items: Vec<Box<dyn Widget>>,
    ) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            item_height,
            selected,
            on_select,
            items,
            scroll_offset: 0,
            focused: false,
        }
    }

    fn total_content_height(&self) -> i32 {
        self.items.len() as i32 * self.item_height
    }

    fn max_scroll_offset(&self) -> i32 {
        (self.total_content_height() - self.rect.height).max(0)
    }

    fn first_visible(&self) -> usize {
        if self.item_height <= 0 {
            return 0;
        }
        (self.scroll_offset / self.item_height) as usize
    }

    fn last_visible(&self) -> usize {
        if self.item_height <= 0 {
            return 0;
        }
        let last = ((self.scroll_offset + self.rect.height) / self.item_height) as usize + 1;
        last.min(self.items.len())
    }

    /// Ensure the selected item is visible by adjusting scroll_offset.
    fn scroll_to_selected(&mut self) {
        if let Some(sel) = self.selected {
            let item_top = sel as i32 * self.item_height;
            let item_bottom = item_top + self.item_height;

            if item_top < self.scroll_offset {
                self.scroll_offset = item_top;
            } else if item_bottom > self.scroll_offset + self.rect.height {
                self.scroll_offset = item_bottom - self.rect.height;
            }

            self.scroll_offset = self.scroll_offset.clamp(0, self.max_scroll_offset());
        }
    }
}

impl Widget for ListViewWidget {
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        // The list fills the available space. Its natural height is the total
        // content height, but we constrain to parent.
        let w = constraints.max_width;
        let h = constraints
            .constrain(Size::new(w, self.total_content_height()))
            .height;
        Size::new(w, h)
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
        self.scroll_offset = self.scroll_offset.clamp(0, self.max_scroll_offset());

        // Measure then layout all item widgets so their internal structure is initialized.
        let item_constraints = BoxConstraints {
            min_width: rect.width,
            max_width: rect.width,
            min_height: self.item_height,
            max_height: self.item_height,
        };
        let style = crate::style::StyleSheet::dark();
        let mut mctx = crate::traits::MeasureCtx { style: &style };
        for (i, item) in self.items.iter_mut().enumerate() {
            let _ = item.measure(item_constraints, &mut mctx);
            let y = rect.y + i as i32 * self.item_height - self.scroll_offset;
            item.layout(Rect::new(rect.x, y, rect.width, self.item_height));
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let first = self.first_visible();
        let last = self.last_visible().min(self.items.len());

        // Clip to our viewport.
        ctx.with_clip(self.rect, |ctx| {
            for i in first..last {
                let y = self.rect.y + i as i32 * self.item_height - self.scroll_offset;
                let item_rect = Rect::new(self.rect.x, y, self.rect.width, self.item_height);

                // Draw selection highlight.
                if self.selected == Some(i) {
                    ctx.fill_rect(
                        item_rect.x,
                        item_rect.y,
                        item_rect.width,
                        item_rect.height,
                        ctx.style.bg_accent,
                    );
                }

                // Paint item widget.
                self.items[i].paint(ctx);
            }
        });

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

        match event {
            WidgetEvent::Scroll { delta_y, .. } => {
                if *delta_y == 0 {
                    return EventResponse::Ignored;
                }
                let line_height = self.item_height;
                let scroll_lines = if *delta_y > 0 {
                    (delta_y / 120).max(1) * 3
                } else {
                    (delta_y / 120).min(-1) * 3
                };
                let old = self.scroll_offset;
                self.scroll_offset = (self.scroll_offset + scroll_lines * line_height)
                    .clamp(0, self.max_scroll_offset());

                if self.scroll_offset != old {
                    // Re-layout items at new positions.
                    for (i, item) in self.items.iter_mut().enumerate() {
                        let y = self.rect.y + i as i32 * self.item_height - self.scroll_offset;
                        item.layout(Rect::new(self.rect.x, y, self.rect.width, self.item_height));
                    }
                    EventResponse::Consumed
                } else {
                    EventResponse::Ignored
                }
            }

            WidgetEvent::PointerDown { y, .. } => {
                if self.item_height <= 0 {
                    return EventResponse::Ignored;
                }
                let relative_y = *y - self.rect.y + self.scroll_offset;
                let index = (relative_y / self.item_height) as usize;
                if index < self.items.len() {
                    self.selected = Some(index);
                    if let Some(cb) = &self.on_select {
                        sink.emit_raw(cb(index));
                    }
                    EventResponse::Consumed
                } else {
                    EventResponse::Ignored
                }
            }

            WidgetEvent::KeyDown { key, .. } => match key {
                Key::Named(NamedKey::Up) => {
                    if let Some(sel) = self.selected {
                        if sel > 0 {
                            self.selected = Some(sel - 1);
                            self.scroll_to_selected();
                            if let Some(cb) = &self.on_select {
                                sink.emit_raw(cb(sel - 1));
                            }
                            return EventResponse::Consumed;
                        }
                    } else if !self.items.is_empty() {
                        self.selected = Some(0);
                        self.scroll_to_selected();
                        if let Some(cb) = &self.on_select {
                            sink.emit_raw(cb(0));
                        }
                        return EventResponse::Consumed;
                    }
                    EventResponse::Ignored
                }
                Key::Named(NamedKey::Down) => {
                    if let Some(sel) = self.selected {
                        if sel + 1 < self.items.len() {
                            self.selected = Some(sel + 1);
                            self.scroll_to_selected();
                            if let Some(cb) = &self.on_select {
                                sink.emit_raw(cb(sel + 1));
                            }
                            return EventResponse::Consumed;
                        }
                    } else if !self.items.is_empty() {
                        self.selected = Some(0);
                        self.scroll_to_selected();
                        if let Some(cb) = &self.on_select {
                            sink.emit_raw(cb(0));
                        }
                        return EventResponse::Consumed;
                    }
                    EventResponse::Ignored
                }
                Key::Named(NamedKey::Home) => {
                    if !self.items.is_empty() {
                        self.selected = Some(0);
                        self.scroll_to_selected();
                        if let Some(cb) = &self.on_select {
                            sink.emit_raw(cb(0));
                        }
                        EventResponse::Consumed
                    } else {
                        EventResponse::Ignored
                    }
                }
                Key::Named(NamedKey::End) => {
                    if !self.items.is_empty() {
                        let last = self.items.len() - 1;
                        self.selected = Some(last);
                        self.scroll_to_selected();
                        if let Some(cb) = &self.on_select {
                            sink.emit_raw(cb(last));
                        }
                        EventResponse::Consumed
                    } else {
                        EventResponse::Ignored
                    }
                }
                _ => EventResponse::Ignored,
            },

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
        Role::List
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
        &self.items
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.items
    }
}
