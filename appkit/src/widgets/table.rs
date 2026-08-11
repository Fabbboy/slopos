use std::any::Any;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent,
};
use crate::node::{ContextMenuAt, SortIndicator, TableColumn, TableColumnWidth};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

use slopos_abi::draw::Color32;

/// Multi-column table with fixed row height, virtual scrolling, and keyboard navigation.
pub struct TableWidget {
    id: WidgetId,
    rect: Rect,
    columns: Vec<TableColumn>,
    rows: Vec<Vec<Box<dyn Widget>>>,
    row_height: i32,
    selected: Option<usize>,
    hovered_row: Option<usize>,
    on_select: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    on_header_click: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    on_context_menu: Option<Box<dyn Fn(ContextMenuAt) -> Box<dyn Any>>>,
    scroll_offset: i32,
    header_height: i32,
    col_widths: Vec<i32>,
    focused: bool,
}

impl TableWidget {
    pub fn new(
        columns: Vec<TableColumn>,
        rows: Vec<Vec<Box<dyn Widget>>>,
        row_height: i32,
        selected: Option<usize>,
        on_select: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
        on_header_click: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
        on_context_menu: Option<Box<dyn Fn(ContextMenuAt) -> Box<dyn Any>>>,
    ) -> Self {
        let col_count = columns.len();
        #[cfg(debug_assertions)]
        for (i, row) in rows.iter().enumerate() {
            debug_assert_eq!(
                row.len(),
                col_count,
                "Table row {} has {} cells but {} columns",
                i,
                row.len(),
                col_count
            );
        }
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            columns,
            rows,
            row_height,
            selected,
            hovered_row: None,
            on_select,
            on_header_click,
            on_context_menu,
            scroll_offset: 0,
            header_height: row_height,
            col_widths: vec![0; col_count],
            focused: false,
        }
    }

    fn row_count(&self) -> usize {
        self.rows.len()
    }

    fn body_height(&self) -> i32 {
        (self.rect.height - self.header_height).max(0)
    }

    fn total_content_height(&self) -> i32 {
        self.row_count() as i32 * self.row_height
    }

    fn max_scroll_offset(&self) -> i32 {
        (self.total_content_height() - self.body_height()).max(0)
    }

    /// Resolve column pixel widths from the TableColumnWidth specs.
    fn resolve_col_widths(&mut self, available: i32) {
        let mut fixed_total = 0i32;
        let mut flex_total = 0u16;

        for col in &self.columns {
            match col.width {
                TableColumnWidth::Fixed(px) => fixed_total += px,
                TableColumnWidth::Flex(weight) => flex_total += weight,
            }
        }

        let flex_space = (available - fixed_total).max(0);

        self.col_widths.clear();
        for col in &self.columns {
            let w = match col.width {
                TableColumnWidth::Fixed(px) => px,
                TableColumnWidth::Flex(weight) => {
                    if flex_total > 0 {
                        (flex_space as i64 * weight as i64 / flex_total as i64) as i32
                    } else {
                        0
                    }
                }
            };
            self.col_widths.push(w);
        }
    }

    /// Ensure the selected row is visible by adjusting scroll_offset.
    fn scroll_to_selected(&mut self) {
        if let Some(sel) = self.selected {
            let item_top = sel as i32 * self.row_height;
            let item_bottom = item_top + self.row_height;
            let body_h = self.body_height();

            if item_top < self.scroll_offset {
                self.scroll_offset = item_top;
            } else if item_bottom > self.scroll_offset + body_h {
                self.scroll_offset = item_bottom - body_h;
            }

            self.scroll_offset = self.scroll_offset.clamp(0, self.max_scroll_offset());
        }
    }

    /// Return the column x-offset (relative to rect.x) for a given column index.
    fn col_x_offset(&self, col_idx: usize) -> i32 {
        self.col_widths[..col_idx].iter().sum()
    }

    /// Determine which column index a given x coordinate (window-space) falls in.
    fn column_at_x(&self, x: i32) -> Option<usize> {
        let rel_x = x - self.rect.x;
        let mut acc = 0;
        for (i, &w) in self.col_widths.iter().enumerate() {
            if rel_x >= acc && rel_x < acc + w {
                return Some(i);
            }
            acc += w;
        }
        None
    }

    /// Visible row range for virtual scrolling.
    fn visible_range(&self) -> (usize, usize) {
        if self.row_height <= 0 {
            return (0, 0);
        }
        let body_h = self.body_height();
        let start = (self.scroll_offset / self.row_height) as usize;
        let end = ((self.scroll_offset + body_h) / self.row_height + 1).min(self.row_count() as i32)
            as usize;
        (start, end)
    }

    /// Number of rows visible in the body area (for PageUp/PageDown).
    fn page_size(&self) -> usize {
        if self.row_height <= 0 {
            return 1;
        }
        (self.body_height() / self.row_height).max(1) as usize
    }

    /// Row index at window-space `y`, or `None` outside the body rows.
    fn row_at_y(&self, y: i32) -> Option<usize> {
        if self.row_height <= 0 {
            return None;
        }
        let body_top = self.rect.y + self.header_height;
        if y < body_top {
            return None;
        }
        let index = ((y - body_top + self.scroll_offset) / self.row_height) as usize;
        (index < self.row_count()).then_some(index)
    }

    /// Select `row` and emit, skipping the emit when it is already selected so
    /// a repeated click does not churn the app's state.
    fn select_row(&mut self, row: usize, sink: &mut MessageSink) {
        if self.selected == Some(row) {
            return;
        }
        self.selected = Some(row);
        if let Some(cb) = &self.on_select {
            sink.emit_raw(cb(row));
        }
    }

    /// Bottom-left corner of `row` in window coordinates, clamped into the body
    /// so a partially-scrolled row still anchors a popup somewhere visible.
    fn row_anchor(&self, row: usize) -> (i32, i32) {
        let body_top = self.rect.y + self.header_height;
        let y = body_top + row as i32 * self.row_height - self.scroll_offset + self.row_height;
        let bottom = self.rect.y + self.rect.height;
        (self.rect.x, y.clamp(body_top, bottom))
    }

    fn emit_context_menu(&self, row: usize, x: i32, y: i32, sink: &mut MessageSink) -> bool {
        match &self.on_context_menu {
            Some(cb) => {
                sink.emit_raw(cb(ContextMenuAt { row, x, y }));
                true
            }
            None => false,
        }
    }
}

impl Widget for TableWidget {
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let w = constraints.max_width;
        let h = constraints.max_height;
        self.resolve_col_widths(w);
        Size::new(w, h)
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
        self.resolve_col_widths(rect.width);
        self.scroll_offset = self.scroll_offset.clamp(0, self.max_scroll_offset());

        // Measure then layout each cell widget at its absolute position.
        let cell_padding = 4;
        let style = crate::style::StyleSheet::dark();
        let mut mctx = crate::traits::MeasureCtx { style: &style };
        for (row_idx, row) in self.rows.iter_mut().enumerate() {
            let y =
                rect.y + self.header_height + row_idx as i32 * self.row_height - self.scroll_offset;
            let mut col_x = rect.x;
            for (col_idx, cell) in row.iter_mut().enumerate() {
                let cw = self.col_widths.get(col_idx).copied().unwrap_or(0);
                let cell_w = (cw - cell_padding * 2).max(0);
                let cell_constraints = BoxConstraints {
                    min_width: cell_w,
                    max_width: cell_w,
                    min_height: self.row_height,
                    max_height: self.row_height,
                };
                let _ = cell.measure(cell_constraints, &mut mctx);
                cell.layout(Rect::new(col_x + cell_padding, y, cell_w, self.row_height));
                col_x += cw;
            }
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let style = ctx.style;
        let cell_padding = 4;

        // --- Header row ---
        ctx.fill_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.header_height,
            style.bg_secondary,
        );

        let text_y = self.rect.y + (self.header_height - ctx.text_height()) / 2;
        let mut hx = self.rect.x;
        for (i, col) in self.columns.iter().enumerate() {
            let cw = self.col_widths.get(i).copied().unwrap_or(0);
            // Build label with sort indicator.
            let label = match col.sort_indicator {
                Some(SortIndicator::Ascending) => {
                    let mut s = col.label.clone();
                    s.push_str(" ^");
                    s
                }
                Some(SortIndicator::Descending) => {
                    let mut s = col.label.clone();
                    s.push_str(" v");
                    s
                }
                None => col.label.clone(),
            };
            ctx.draw_text_transparent(hx + cell_padding, text_y, &label, style.text_primary);
            hx += cw;
        }

        // Divider line below header.
        ctx.fill_rect(
            self.rect.x,
            self.rect.y + self.header_height - 1,
            self.rect.width,
            1,
            style.border_divider,
        );

        // --- Body rows (virtualized) ---
        let body_rect = Rect::new(
            self.rect.x,
            self.rect.y + self.header_height,
            self.rect.width,
            self.body_height(),
        );
        let (vis_start, vis_end) = self.visible_range();

        ctx.with_clip(body_rect, |ctx| {
            for i in vis_start..vis_end.min(self.rows.len()) {
                let y = self.rect.y + self.header_height + i as i32 * self.row_height
                    - self.scroll_offset;

                // Row background: selected, even, or odd.
                let bg = if self.selected == Some(i) {
                    Color32::new(
                        style.bg_accent.red(),
                        style.bg_accent.green(),
                        style.bg_accent.blue(),
                        100,
                    )
                } else if self.hovered_row == Some(i) {
                    style.bg_tertiary
                } else if i % 2 == 0 {
                    style.bg_primary
                } else {
                    // Slightly lighter for odd rows.
                    Color32::rgb(
                        style.bg_primary.red().saturating_add(5),
                        style.bg_primary.green().saturating_add(5),
                        style.bg_primary.blue().saturating_add(5),
                    )
                };

                if self.selected == Some(i) {
                    ctx.fill_rect_blended(self.rect.x, y, self.rect.width, self.row_height, bg);
                } else {
                    ctx.fill_rect(self.rect.x, y, self.rect.width, self.row_height, bg);
                }

                // Hover highlight overlay (subtle, on top of bg).
                if self.hovered_row == Some(i) && self.selected != Some(i) {
                    ctx.fill_rect_blended(
                        self.rect.x,
                        y,
                        self.rect.width,
                        self.row_height,
                        Color32::new(255, 255, 255, 15),
                    );
                }

                // Paint cell widgets.
                for cell in &self.rows[i] {
                    cell.paint(ctx);
                }
            }
        });

        // --- Scrollbar ---
        let total_h = self.total_content_height();
        let body_h = self.body_height();
        if total_h > body_h {
            let sb_width = style.scrollbar_width;
            let thumb_min = style.scrollbar_thumb_min;

            let track_x = self.rect.x + self.rect.width - sb_width;
            let track_y = self.rect.y + self.header_height;
            let track_h = body_h;

            // Track background.
            ctx.fill_rect(track_x, track_y, sb_width, track_h, style.bg_secondary);

            // Thumb.
            let max_off = self.max_scroll_offset();
            let thumb_size = if max_off > 0 && total_h > 0 {
                ((body_h as i64 * track_h as i64) / total_h as i64) as i32
            } else {
                track_h
            }
            .max(thumb_min)
            .min(track_h);

            let thumb_pos = if max_off > 0 {
                ((self.scroll_offset as i64 * (track_h - thumb_size) as i64) / max_off as i64)
                    as i32
            } else {
                0
            };

            ctx.fill_rect(
                track_x,
                track_y + thumb_pos,
                sb_width,
                thumb_size,
                style.bg_tertiary,
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

        match event {
            WidgetEvent::PointerDown { x, y, button } => {
                // Header: sorting is a primary-button action; a secondary
                // click there addresses no row, so it has nothing to open.
                if *y >= self.rect.y && *y < self.rect.y + self.header_height {
                    if *button != PointerButton::Left {
                        return EventResponse::Ignored;
                    }
                    if let Some(cb) = &self.on_header_click {
                        if let Some(col) = self.column_at_x(*x) {
                            sink.emit_raw(cb(col));
                        }
                    }
                    return EventResponse::Consumed;
                }

                let Some(index) = self.row_at_y(*y) else {
                    return EventResponse::Ignored;
                };

                match button {
                    PointerButton::Left => {
                        self.select_row(index, sink);
                        EventResponse::Consumed
                    }
                    // Secondary click selects first: the menu that opens acts on
                    // the selection, so the two must never disagree.
                    PointerButton::Right => {
                        self.select_row(index, sink);
                        if self.emit_context_menu(index, *x, *y, sink) {
                            EventResponse::Consumed
                        } else {
                            EventResponse::Ignored
                        }
                    }
                    PointerButton::Middle => EventResponse::Ignored,
                }
            }

            WidgetEvent::PointerMove { x: _, y } => {
                let body_top = self.rect.y + self.header_height;
                let old_hover = self.hovered_row;
                if *y >= body_top && self.row_height > 0 {
                    let relative_y = *y - body_top + self.scroll_offset;
                    let index = (relative_y / self.row_height) as usize;
                    self.hovered_row = if index < self.row_count() {
                        Some(index)
                    } else {
                        None
                    };
                } else {
                    self.hovered_row = None;
                }
                if self.hovered_row != old_hover {
                    EventResponse::Consumed
                } else {
                    EventResponse::Ignored
                }
            }

            WidgetEvent::PointerLeave => {
                if self.hovered_row.is_some() {
                    self.hovered_row = None;
                    EventResponse::Consumed
                } else {
                    EventResponse::Ignored
                }
            }

            WidgetEvent::Scroll { delta_y, .. } => {
                if *delta_y == 0 {
                    return EventResponse::Ignored;
                }
                let old = self.scroll_offset;
                self.scroll_offset =
                    (self.scroll_offset + *delta_y).clamp(0, self.max_scroll_offset());

                if self.scroll_offset != old {
                    self.relayout_rows();
                    EventResponse::Consumed
                } else {
                    EventResponse::Ignored
                }
            }

            WidgetEvent::KeyDown { key, modifiers, .. } => {
                let rc = self.row_count();
                if rc == 0 {
                    return EventResponse::Ignored;
                }

                // The keyboard equivalents of a secondary click. Shift+F10 is
                // the fallback for keyboards with no dedicated Menu key.
                let context_key = matches!(key, Key::Named(NamedKey::Menu))
                    || (matches!(key, Key::Named(NamedKey::F10)) && modifiers.shift);
                if context_key {
                    let Some(row) = self.selected else {
                        return EventResponse::Ignored;
                    };
                    let (ax, ay) = self.row_anchor(row);
                    return if self.emit_context_menu(row, ax, ay, sink) {
                        EventResponse::Consumed
                    } else {
                        EventResponse::Ignored
                    };
                }

                match key {
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
                        } else {
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
                            if sel + 1 < rc {
                                self.selected = Some(sel + 1);
                                self.scroll_to_selected();
                                if let Some(cb) = &self.on_select {
                                    sink.emit_raw(cb(sel + 1));
                                }
                                return EventResponse::Consumed;
                            }
                        } else {
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
                        self.selected = Some(0);
                        self.scroll_to_selected();
                        if let Some(cb) = &self.on_select {
                            sink.emit_raw(cb(0));
                        }
                        EventResponse::Consumed
                    }
                    Key::Named(NamedKey::End) => {
                        self.selected = Some(rc - 1);
                        self.scroll_to_selected();
                        if let Some(cb) = &self.on_select {
                            sink.emit_raw(cb(rc - 1));
                        }
                        EventResponse::Consumed
                    }
                    Key::Named(NamedKey::PageUp) => {
                        let page = self.page_size();
                        let sel = self.selected.unwrap_or(0);
                        let new_sel = sel.saturating_sub(page);
                        self.selected = Some(new_sel);
                        self.scroll_to_selected();
                        if let Some(cb) = &self.on_select {
                            sink.emit_raw(cb(new_sel));
                        }
                        EventResponse::Consumed
                    }
                    Key::Named(NamedKey::PageDown) => {
                        let page = self.page_size();
                        let sel = self.selected.unwrap_or(0);
                        let new_sel = (sel + page).min(rc - 1);
                        self.selected = Some(new_sel);
                        self.scroll_to_selected();
                        if let Some(cb) = &self.on_select {
                            sink.emit_raw(cb(new_sel));
                        }
                        EventResponse::Consumed
                    }
                    _ => EventResponse::Ignored,
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
}

impl TableWidget {
    /// Re-layout all row cells after a scroll offset change.
    fn relayout_rows(&mut self) {
        let cell_padding = 4;
        for (row_idx, row) in self.rows.iter_mut().enumerate() {
            let y = self.rect.y + self.header_height + row_idx as i32 * self.row_height
                - self.scroll_offset;
            let mut col_x = self.rect.x;
            for (col_idx, cell) in row.iter_mut().enumerate() {
                let cw = self.col_widths.get(col_idx).copied().unwrap_or(0);
                cell.layout(Rect::new(
                    col_x + cell_padding,
                    y,
                    (cw - cell_padding * 2).max(0),
                    self.row_height,
                ));
                col_x += cw;
            }
        }
    }
}
