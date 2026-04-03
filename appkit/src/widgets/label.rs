use crate::constraints::{BoxConstraints, Rect, Size, TextAlignment};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

pub struct LabelWidget {
    id: WidgetId,
    rect: Rect,
    text: String,
    alignment: TextAlignment,
    wrap: bool,
    max_lines: Option<u32>,
}

impl LabelWidget {
    pub fn new(text: String, alignment: TextAlignment, wrap: bool, max_lines: Option<u32>) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            text,
            alignment,
            wrap,
            max_lines,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
    }

    /// Word-wrap `text` into lines that each fit within `max_width` pixels.
    fn wrap_lines(&self, text: &str, max_width: i32, ctx: &PaintContext) -> Vec<String> {
        let mut lines = Vec::new();
        for raw_line in text.split('\n') {
            if raw_line.is_empty() {
                lines.push(String::new());
                continue;
            }
            let mut current = String::new();
            for word in raw_line.split_whitespace() {
                if current.is_empty() {
                    // First word on the line — always accept it even if it overflows.
                    current.push_str(word);
                } else {
                    let candidate = format!("{} {}", current, word);
                    if ctx.text_width(&candidate) <= max_width {
                        current = candidate;
                    } else {
                        lines.push(current);
                        current = word.to_string();
                    }
                }
            }
            lines.push(current);
        }
        if let Some(max) = self.max_lines {
            lines.truncate(max as usize);
        }
        lines
    }
}

impl Widget for LabelWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let line_height = ctx.style.line_height;

        if !self.wrap {
            // Single-line: measure natural text width.
            // Use PaintContext::text_width indirectly via the font module.
            let text_w = crate::text::string_width(&self.text);
            let mut lines = 1i32;
            if self.text.contains('\n') {
                lines = self.text.split('\n').count() as i32;
            }
            if let Some(max) = self.max_lines {
                lines = lines.min(max as i32);
            }
            let size = Size::new(text_w, line_height * lines);
            constraints.constrain(size)
        } else {
            // Wrapping: use constraint max width for layout.
            let avail_w = constraints.max_width;
            // Count wrapped lines using a lightweight pass.
            let mut line_count = 0u32;
            for raw_line in self.text.split('\n') {
                if raw_line.is_empty() {
                    line_count += 1;
                    continue;
                }
                let mut current_w = 0i32;
                let mut on_line = false;
                for word in raw_line.split_whitespace() {
                    let word_w = crate::text::string_width(word);
                    if !on_line {
                        current_w = word_w;
                        on_line = true;
                    } else {
                        let space_w = crate::text::string_width(" ");
                        if current_w + space_w + word_w <= avail_w {
                            current_w += space_w + word_w;
                        } else {
                            line_count += 1;
                            current_w = word_w;
                        }
                    }
                }
                if on_line {
                    line_count += 1;
                }
            }
            if line_count == 0 {
                line_count = 1;
            }
            if let Some(max) = self.max_lines {
                line_count = line_count.min(max);
            }
            let size = Size::new(avail_w, line_height * line_count as i32);
            constraints.constrain(size)
        }
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let fg = ctx.style.text_primary;
        let line_height = ctx.style.line_height;

        if !self.wrap {
            // Paint each hard-newline-separated line.
            let lines: Vec<&str> = self.text.split('\n').collect();
            let max = self
                .max_lines
                .map(|m| m as usize)
                .unwrap_or(lines.len())
                .min(lines.len());
            for (i, line) in lines[..max].iter().enumerate() {
                let tw = ctx.text_width(line);
                let x = match self.alignment {
                    TextAlignment::Start => self.rect.x,
                    TextAlignment::Center => self.rect.x + (self.rect.width - tw) / 2,
                    TextAlignment::End => self.rect.x + self.rect.width - tw,
                };
                let y = self.rect.y + i as i32 * line_height;
                ctx.draw_text_transparent(x, y, line, fg);
            }
        } else {
            let lines = self.wrap_lines(&self.text, self.rect.width, ctx);
            for (i, line) in lines.iter().enumerate() {
                let tw = ctx.text_width(line);
                let x = match self.alignment {
                    TextAlignment::Start => self.rect.x,
                    TextAlignment::Center => self.rect.x + (self.rect.width - tw) / 2,
                    TextAlignment::End => self.rect.x + self.rect.width - tw,
                };
                let y = self.rect.y + i as i32 * line_height;
                ctx.draw_text_transparent(x, y, line, fg);
            }
        }
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
        Role::Label
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.text)
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
