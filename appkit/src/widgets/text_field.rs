use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent,
};
use crate::node::MessageId;
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

pub struct TextFieldWidget {
    id: WidgetId,
    rect: Rect,
    text: String,
    placeholder: String,
    on_change: MessageId,
    max_length: Option<usize>,
    read_only: bool,
    cursor: usize,
    selection_anchor: Option<usize>,
    scroll_offset: i32,
    focused: bool,
    blink_on: bool,
}

impl TextFieldWidget {
    pub fn new(
        text: String,
        placeholder: String,
        on_change: MessageId,
        max_length: Option<usize>,
        read_only: bool,
    ) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            text,
            placeholder,
            on_change,
            max_length,
            read_only,
            cursor: 0,
            selection_anchor: None,
            scroll_offset: 0,
            focused: false,
            blink_on: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
        let len = self.char_len();
        if self.cursor > len {
            self.cursor = len;
        }
        self.selection_anchor = None;
        self.ensure_cursor_visible();
    }

    pub fn on_change(&self) -> MessageId {
        self.on_change
    }

    // --- Character helpers ---

    fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    /// Byte offset for the n-th character boundary.
    fn char_to_byte(&self, idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    /// Substring as String from char index range [start..end).
    fn char_substring(&self, start: usize, end: usize) -> String {
        self.text.chars().skip(start).take(end - start).collect()
    }

    // --- Cursor positioning ---

    /// Pixel x-offset of the cursor at char index `idx` relative to text start.
    fn char_x_offset(&self, idx: usize) -> i32 {
        let prefix: String = self.text.chars().take(idx).collect();
        crate::text::string_width(&prefix)
    }

    /// Adjust scroll_offset so the cursor is within the visible content area.
    fn ensure_cursor_visible(&mut self) {
        let padding_h = 8; // field_padding_h from StyleSheet::dark()
        let content_width = self.rect.width - padding_h * 2;
        if content_width <= 0 {
            return;
        }
        let cx = self.char_x_offset(self.cursor);
        // Cursor is left of visible area.
        if cx - self.scroll_offset < 0 {
            self.scroll_offset = cx;
        }
        // Cursor is right of visible area (leave 2px for cursor bar).
        if cx - self.scroll_offset > content_width - 2 {
            self.scroll_offset = cx - content_width + 2;
        }
    }

    /// Map a pixel x coordinate (window-space) to the nearest char index.
    fn x_to_char_index(&self, x: i32) -> usize {
        let padding_h = 8;
        let local_x = x - self.rect.x - padding_h + self.scroll_offset;
        let len = self.char_len();
        if local_x <= 0 {
            return 0;
        }
        // Walk characters to find the closest boundary.
        let mut prev_x = 0i32;
        for i in 1..=len {
            let cx = self.char_x_offset(i);
            let mid = (prev_x + cx) / 2;
            if local_x < mid {
                return i - 1;
            }
            prev_x = cx;
        }
        len
    }

    // --- Selection helpers ---

    /// Returns ordered (start, end) of the selection, or None.
    fn selection_range(&self) -> Option<(usize, usize)> {
        self.selection_anchor.map(|anchor| {
            let a = anchor.min(self.cursor);
            let b = anchor.max(self.cursor);
            (a, b)
        })
    }

    /// Delete selected text, move cursor to start of selection.
    fn delete_selection(&mut self) {
        if let Some((start, end)) = self.selection_range() {
            let byte_start = self.char_to_byte(start);
            let byte_end = self.char_to_byte(end);
            self.text.replace_range(byte_start..byte_end, "");
            self.cursor = start;
            self.selection_anchor = None;
        }
    }

    /// Return the selected substring, if any.
    fn selected_text(&self) -> Option<String> {
        self.selection_range()
            .map(|(start, end)| self.char_substring(start, end))
    }

    /// Move cursor, optionally extending selection with Shift.
    fn move_cursor(&mut self, new_pos: usize, extend_selection: bool) {
        if extend_selection {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
        self.cursor = new_pos;
        self.blink_on = true;
        self.ensure_cursor_visible();
    }

    /// Insert text at cursor (replacing selection if any). Respects max_length.
    fn insert_text(&mut self, s: &str) {
        if self.read_only {
            return;
        }
        // Delete selection first.
        self.delete_selection();

        let insert_chars: Vec<char> = s.chars().collect();
        let mut count = insert_chars.len();

        // Enforce max_length.
        if let Some(max) = self.max_length {
            let current = self.char_len();
            if current >= max {
                return;
            }
            count = count.min(max - current);
        }

        let byte_pos = self.char_to_byte(self.cursor);
        let to_insert: String = insert_chars[..count].iter().collect();
        self.text.insert_str(byte_pos, &to_insert);
        self.cursor += count;
        self.blink_on = true;
        self.ensure_cursor_visible();
    }

    // --- Content area rect (for clipping) ---

    fn content_rect(&self, style: &crate::style::StyleSheet) -> Rect {
        let ph = style.field_padding_h;
        let pv = style.field_padding_v;
        Rect::new(
            self.rect.x + ph,
            self.rect.y + pv,
            (self.rect.width - ph * 2).max(0),
            (self.rect.height - pv * 2).max(0),
        )
    }
}

impl Widget for TextFieldWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let text_h = crate::text::cell_height();
        let width = constraints.max_width.max(ctx.style.field_min_width);
        let height = text_h + ctx.style.field_padding_v * 2;
        constraints.constrain(Size::new(width, height))
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
        self.ensure_cursor_visible();
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let style = ctx.style;
        let radius = style.corner_radius;
        let ph = style.field_padding_h;
        let pv = style.field_padding_v;

        // 1. Background
        ctx.fill_rounded_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            radius,
            style.bg_secondary,
        );

        // 2. Border
        let border_color = if self.focused {
            style.border_focused
        } else {
            style.border_default
        };
        ctx.draw_rounded_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            radius,
            border_color,
        );

        // Content area for clipping.
        let content = self.content_rect(style);
        let text_y = self.rect.y + pv;
        let text_x = self.rect.x + ph - self.scroll_offset;

        ctx.with_clip(content, |ctx| {
            // 3. Selection highlight
            if let Some((start, end)) = self.selection_range() {
                let sel_x = text_x + self.char_x_offset(start);
                let sel_end_x = text_x + self.char_x_offset(end);
                let sel_w = sel_end_x - sel_x;
                // Semi-transparent accent for selection background.
                let sel_color = slopos_abi::draw::Color32::new(0, 122, 255, 100);
                ctx.fill_rect_blended(sel_x, text_y, sel_w, ctx.text_height(), sel_color);
            }

            // 4. Text or placeholder
            if self.text.is_empty() && !self.focused {
                ctx.draw_text_transparent(
                    text_x,
                    text_y,
                    &self.placeholder,
                    ctx.style.text_secondary,
                );
            } else {
                ctx.draw_text_transparent(text_x, text_y, &self.text, ctx.style.text_primary);
            }

            // 5. Cursor
            if self.focused && self.blink_on {
                let cursor_x = text_x + self.char_x_offset(self.cursor);
                ctx.fill_rect(
                    cursor_x,
                    text_y,
                    2,
                    ctx.text_height(),
                    ctx.style.text_primary,
                );
            }
        });

        // 6. Focus ring
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
        if phase != EventPhase::Target {
            return EventResponse::Ignored;
        }

        match event {
            WidgetEvent::TextInput { character } => {
                if self.read_only {
                    return EventResponse::Consumed;
                }
                // Filter out control characters.
                if character.is_control() {
                    return EventResponse::Ignored;
                }
                self.insert_text(&character.to_string());
                sink.emit(self.on_change);
                EventResponse::Consumed
            }

            WidgetEvent::KeyDown { key, modifiers, .. } => {
                let resp = self.handle_key_down(key, modifiers);
                if resp.is_consumed() && self.is_text_modifying_key(key, modifiers) {
                    sink.emit(self.on_change);
                }
                resp
            }

            WidgetEvent::PointerDown { x, y: _, button } => {
                if *button != PointerButton::Left {
                    return EventResponse::Ignored;
                }
                let idx = self.x_to_char_index(*x);
                self.cursor = idx;
                self.selection_anchor = None;
                self.blink_on = true;
                self.ensure_cursor_visible();
                EventResponse::CapturePointer
            }

            WidgetEvent::PointerMove { x, .. } => {
                // Drag-select: extend selection to pointer position.
                let idx = self.x_to_char_index(*x);
                if self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor);
                }
                self.cursor = idx;
                self.blink_on = true;
                self.ensure_cursor_visible();
                EventResponse::Consumed
            }

            WidgetEvent::PointerUp { .. } => EventResponse::ReleasePointer,

            WidgetEvent::FocusGained => {
                self.focused = true;
                self.blink_on = true;
                EventResponse::Consumed
            }

            WidgetEvent::FocusLost => {
                self.focused = false;
                self.selection_anchor = None;
                EventResponse::Consumed
            }

            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::TextField
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.placeholder)
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

impl TextFieldWidget {
    fn handle_key_down(&mut self, key: &Key, modifiers: &Modifiers) -> EventResponse {
        match key {
            // Ctrl+A: select all
            Key::Char('a') if modifiers.ctrl => {
                self.selection_anchor = Some(0);
                self.cursor = self.char_len();
                self.blink_on = true;
                self.ensure_cursor_visible();
                EventResponse::Consumed
            }

            Key::Named(NamedKey::Backspace) => {
                if self.read_only {
                    return EventResponse::Consumed;
                }
                if self.selection_anchor.is_some() {
                    self.delete_selection();
                } else if self.cursor > 0 {
                    let byte_start = self.char_to_byte(self.cursor - 1);
                    let byte_end = self.char_to_byte(self.cursor);
                    self.text.replace_range(byte_start..byte_end, "");
                    self.cursor -= 1;
                }
                self.blink_on = true;
                self.ensure_cursor_visible();
                EventResponse::Consumed
            }

            Key::Named(NamedKey::Delete) => {
                if self.read_only {
                    return EventResponse::Consumed;
                }
                if self.selection_anchor.is_some() {
                    self.delete_selection();
                } else if self.cursor < self.char_len() {
                    let byte_start = self.char_to_byte(self.cursor);
                    let byte_end = self.char_to_byte(self.cursor + 1);
                    self.text.replace_range(byte_start..byte_end, "");
                }
                self.blink_on = true;
                self.ensure_cursor_visible();
                EventResponse::Consumed
            }

            Key::Named(NamedKey::Left) => {
                let new_pos = if modifiers.shift {
                    self.cursor.saturating_sub(1)
                } else if let Some((start, _)) = self.selection_range() {
                    // Without shift, collapse selection to its start side.
                    start
                } else {
                    self.cursor.saturating_sub(1)
                };
                self.move_cursor(new_pos, modifiers.shift);
                EventResponse::Consumed
            }

            Key::Named(NamedKey::Right) => {
                let len = self.char_len();
                let new_pos = if modifiers.shift {
                    (self.cursor + 1).min(len)
                } else if let Some((_, end)) = self.selection_range() {
                    end
                } else {
                    (self.cursor + 1).min(len)
                };
                self.move_cursor(new_pos, modifiers.shift);
                EventResponse::Consumed
            }

            Key::Named(NamedKey::Home) => {
                self.move_cursor(0, modifiers.shift);
                EventResponse::Consumed
            }

            Key::Named(NamedKey::End) => {
                let len = self.char_len();
                self.move_cursor(len, modifiers.shift);
                EventResponse::Consumed
            }

            _ => EventResponse::Ignored,
        }
    }

    fn is_text_modifying_key(&self, key: &Key, _modifiers: &Modifiers) -> bool {
        matches!(
            key,
            Key::Named(NamedKey::Backspace) | Key::Named(NamedKey::Delete)
        )
    }
}
