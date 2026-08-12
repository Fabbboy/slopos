use std::any::Any;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventPhase, EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore, place_widget};

/// Virtualized list with fixed item height.
///
/// Only items in the visible range are painted, keeping performance
/// constant regardless of total item count.
pub struct ListViewWidget {
    core: WidgetCore,
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
            core: WidgetCore::new(),
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
        (self.total_content_height() - self.layout_rect().height).max(0)
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
        let last =
            ((self.scroll_offset + self.layout_rect().height) / self.item_height) as usize + 1;
        last.min(self.items.len())
    }

    /// Position every item row at the current scroll offset.
    fn place_items(&mut self) {
        let rect = self.layout_rect();
        for (i, item) in self.items.iter_mut().enumerate() {
            let y = rect.y + i as i32 * self.item_height - self.scroll_offset;
            place_widget(
                item.as_mut(),
                Rect::new(rect.x, y, rect.width, self.item_height),
            );
        }
    }

    /// Ensure the selected item is visible by adjusting scroll_offset.
    fn scroll_to_selected(&mut self) {
        if let Some(sel) = self.selected {
            let item_top = sel as i32 * self.item_height;
            let item_bottom = item_top + self.item_height;
            let height = self.layout_rect().height;

            if item_top < self.scroll_offset {
                self.scroll_offset = item_top;
            } else if item_bottom > self.scroll_offset + height {
                self.scroll_offset = item_bottom - height;
            }

            self.scroll_offset = self.scroll_offset.clamp(0, self.max_scroll_offset());
        }
    }
}

impl Widget for ListViewWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let w = constraints.max_width;
        let h = constraints
            .constrain(Size::new(w, self.total_content_height()))
            .height;

        // Rows are uniform, so measuring them here — rather than during layout
        // — keeps the measure pass the only place a child is sized.
        let item_constraints = BoxConstraints::tight(Size::new(w, self.item_height));
        for item in &mut self.items {
            crate::traits::measure_widget(item.as_mut(), item_constraints, ctx);
        }

        Size::new(w, h)
    }

    fn layout(&mut self, _rect: Rect) {
        self.scroll_offset = self.scroll_offset.clamp(0, self.max_scroll_offset());
        self.place_items();
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let first = self.first_visible();
        let last = self.last_visible().min(self.items.len());

        // Clip to our viewport.
        ctx.with_clip(rect, |ctx| {
            for i in first..last {
                let y = rect.y + i as i32 * self.item_height - self.scroll_offset;
                let item_rect = Rect::new(rect.x, y, rect.width, self.item_height);

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
            ctx.draw_focus_ring(rect);
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
                    self.place_items();
                    EventResponse::Consumed
                } else {
                    EventResponse::Ignored
                }
            }

            WidgetEvent::PointerDown { x, y, .. } => {
                if self.item_height <= 0 || !self.layout_rect().contains(*x, *y) {
                    return EventResponse::Ignored;
                }
                let relative_y = *y - self.layout_rect().y + self.scroll_offset;
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

    fn children(&self) -> &[Box<dyn Widget>] {
        &self.items
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.items
    }
}
