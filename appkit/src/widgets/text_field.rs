use std::any::Any;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent,
};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

pub struct TextFieldWidget {
    core: WidgetCore,
    text: String,
    placeholder: String,
    on_change: Option<Box<dyn Fn(String) -> Box<dyn Any>>>,
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
        on_change: Option<Box<dyn Fn(String) -> Box<dyn Any>>>,
        max_length: Option<usize>,
        read_only: bool,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
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

    fn rect(&self) -> Rect {
        self.core.rect()
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

    /// Char-index range `[start..end)`, not byte offsets.
    fn char_substring(&self, start: usize, end: usize) -> String {
        self.text.chars().skip(start).take(end - start).collect()
    }

    /// Pixel x-offset of the cursor at char index `idx` relative to text start.
    fn char_x_offset(&self, idx: usize) -> i32 {
        let prefix: String = self.text.chars().take(idx).collect();
        crate::text::string_width(&prefix)
    }

    fn ensure_cursor_visible(&mut self) {
        // TODO(tech-debt): hardcoded copy of StyleSheet::field_padding_h — thread
        // the style through to this path instead.
        let padding_h = 8;
        let content_width = self.rect().width - padding_h * 2;
        if content_width <= 0 {
            return;
        }
        let cx = self.char_x_offset(self.cursor);
        if cx - self.scroll_offset < 0 {
            self.scroll_offset = cx;
        }
        // The 2px keeps the cursor bar itself on screen.
        if cx - self.scroll_offset > content_width - 2 {
            self.scroll_offset = cx - content_width + 2;
        }
    }

    /// Map a pixel x coordinate (window-space) to the nearest char index.
    fn x_to_char_index(&self, x: i32) -> usize {
        let padding_h = 8;
        let local_x = x - self.rect().x - padding_h + self.scroll_offset;
        let len = self.char_len();
        if local_x <= 0 {
            return 0;
        }
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

    /// Returns ordered (start, end) of the selection, or None.
    fn selection_range(&self) -> Option<(usize, usize)> {
        self.selection_anchor.map(|anchor| {
            let a = anchor.min(self.cursor);
            let b = anchor.max(self.cursor);
            (a, b)
        })
    }

    /// Deletes the selection and leaves the cursor at its start.
    fn delete_selection(&mut self) {
        if let Some((start, end)) = self.selection_range() {
            let byte_start = self.char_to_byte(start);
            let byte_end = self.char_to_byte(end);
            self.text.replace_range(byte_start..byte_end, "");
            self.cursor = start;
            self.selection_anchor = None;
        }
    }

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
        self.delete_selection();

        let insert_chars: Vec<char> = s.chars().collect();
        let mut count = insert_chars.len();

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

    fn content_rect(&self, style: &crate::style::StyleSheet) -> Rect {
        let ph = style.field_padding_h;
        let pv = style.field_padding_v;
        Rect::new(
            self.rect().x + ph,
            self.rect().y + pv,
            (self.rect().width - ph * 2).max(0),
            (self.rect().height - pv * 2).max(0),
        )
    }
}

impl Widget for TextFieldWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let text_h = crate::text::cell_height();
        let natural = crate::text::string_width(&self.text) + ctx.style.field_padding_h * 2;
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            natural
        }
        .max(ctx.style.field_min_width);
        let height = text_h + ctx.style.field_padding_v * 2;
        constraints.constrain(Size::new(width, height))
    }

    fn layout(&mut self, _rect: Rect) {
        self.ensure_cursor_visible();
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let style = ctx.style;
        let radius = style.corner_radius;
        let ph = style.field_padding_h;
        let pv = style.field_padding_v;

        ctx.fill_rounded_rect(
            self.rect().x,
            self.rect().y,
            self.rect().width,
            self.rect().height,
            radius,
            style.bg_secondary,
        );

        let border_color = if self.focused {
            style.border_focused
        } else {
            style.border_default
        };
        ctx.draw_rounded_rect(
            self.rect().x,
            self.rect().y,
            self.rect().width,
            self.rect().height,
            radius,
            border_color,
        );

        let content = self.content_rect(style);
        let text_y = self.rect().y + pv;
        let text_x = self.rect().x + ph - self.scroll_offset;

        ctx.with_clip(content, |ctx| {
            if let Some((start, end)) = self.selection_range() {
                let sel_x = text_x + self.char_x_offset(start);
                let sel_end_x = text_x + self.char_x_offset(end);
                let sel_w = sel_end_x - sel_x;
                let sel_color = slopos_abi::draw::Color32::new(0, 122, 255, 100);
                ctx.fill_rect_blended(sel_x, text_y, sel_w, ctx.text_height(), sel_color);
            }

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

        if self.focused {
            ctx.draw_focus_ring(self.rect());
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
                if character.is_control() {
                    return EventResponse::Ignored;
                }
                self.insert_text(&character.to_string());
                if let Some(cb) = &self.on_change {
                    sink.emit_raw(cb(self.text.clone()));
                }
                EventResponse::Consumed
            }

            WidgetEvent::KeyDown { key, modifiers, .. } => {
                let resp = self.handle_key_down(key, modifiers);
                if resp.is_consumed() && self.is_text_modifying_key(key, modifiers) {
                    if let Some(cb) = &self.on_change {
                        sink.emit_raw(cb(self.text.clone()));
                    }
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
                // Drag-select: a move only reaches here while the pointer is
                // captured from PointerDown.
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
}

impl TextFieldWidget {
    fn handle_key_down(&mut self, key: &Key, modifiers: &Modifiers) -> EventResponse {
        match key {
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
