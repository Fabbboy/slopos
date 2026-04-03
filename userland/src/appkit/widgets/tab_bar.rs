use crate::appkit::constraints::{BoxConstraints, Rect, Size};
use crate::appkit::event::{EventPhase, EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::appkit::node::MessageId;
use crate::appkit::paint::PaintContext;
use crate::appkit::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

/// Tab header bar with panel switching.
///
/// Displays a row of tab labels at the top and the active panel's content below.
pub struct TabBarWidget {
    id: WidgetId,
    rect: Rect,
    tabs: Vec<String>,
    active: usize,
    on_change: MessageId,
    content: Vec<Box<dyn Widget>>,
    focused: bool,
}

impl TabBarWidget {
    pub fn new(
        tabs: Vec<String>,
        active: usize,
        on_change: MessageId,
        content: Vec<Box<dyn Widget>>,
    ) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            tabs,
            active,
            on_change,
            content,
            focused: false,
        }
    }

    /// Compute the X ranges for each tab label in the tab row.
    /// Returns Vec of (x_start, width) pairs.
    fn tab_layout(&self, total_width: i32) -> Vec<(i32, i32)> {
        let count = self.tabs.len();
        if count == 0 {
            return Vec::new();
        }
        // Divide available width equally among tabs.
        let tab_w = total_width / count as i32;
        let mut result = Vec::with_capacity(count);
        for i in 0..count {
            let x = i as i32 * tab_w;
            // Last tab gets remaining pixels to avoid rounding gaps.
            let w = if i == count - 1 {
                total_width - x
            } else {
                tab_w
            };
            result.push((x, w));
        }
        result
    }
}

impl Widget for TabBarWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let tab_height = ctx.style.tab_height;
        let w = constraints.max_width;

        // Measure the active panel to determine its height.
        let panel_height = if let Some(panel) = self.content.get_mut(self.active) {
            let panel_constraints = BoxConstraints {
                min_width: w,
                max_width: w,
                min_height: 0,
                max_height: if constraints.max_height == i32::MAX {
                    i32::MAX
                } else {
                    (constraints.max_height - tab_height).max(0)
                },
            };
            panel.measure(panel_constraints, ctx).height
        } else {
            0
        };

        let total_h = tab_height + panel_height;
        constraints.constrain(Size::new(w, total_h))
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;

        // Layout all content panels (only active is visible, but we lay them all out
        // so that switching tabs doesn't require a full re-layout).
        let tab_height = 36; // Matches style.tab_height default.
        let panel_rect = Rect::new(
            rect.x,
            rect.y + tab_height,
            rect.width,
            (rect.height - tab_height).max(0),
        );

        for panel in &mut self.content {
            panel.layout(panel_rect);
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let tab_height = ctx.style.tab_height;
        let underline_height = 3;

        // Tab row background.
        ctx.fill_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            tab_height,
            ctx.style.bg_secondary,
        );

        // Draw each tab label.
        let layout = self.tab_layout(self.rect.width);
        let text_h = ctx.text_height();

        for (i, (tab_x, tab_w)) in layout.iter().enumerate() {
            let abs_x = self.rect.x + tab_x;

            if i == self.active {
                // Active tab: opaque background.
                ctx.fill_rect(abs_x, self.rect.y, *tab_w, tab_height, ctx.style.bg_primary);

                // Accent underline.
                ctx.fill_rect(
                    abs_x,
                    self.rect.y + tab_height - underline_height,
                    *tab_w,
                    underline_height,
                    ctx.style.bg_accent,
                );
            }

            // Label text, centered in the tab cell.
            let label = &self.tabs[i];
            let text_w = ctx.text_width(label);
            let tx = abs_x + (*tab_w - text_w) / 2;
            let ty = self.rect.y + (tab_height - text_h) / 2;

            let fg = if i == self.active {
                ctx.style.text_primary
            } else {
                ctx.style.text_secondary
            };
            ctx.draw_text_transparent(tx, ty, label, fg);
        }

        // Separator line below tab row.
        ctx.fill_rect(
            self.rect.x,
            self.rect.y + tab_height,
            self.rect.width,
            1,
            ctx.style.border_divider,
        );

        // Paint active panel content.
        if let Some(panel) = self.content.get(self.active) {
            panel.paint(ctx);
        }

        // Focus ring.
        if self.focused {
            let tab_row = Rect::new(self.rect.x, self.rect.y, self.rect.width, tab_height);
            ctx.draw_focus_ring(tab_row);
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
            WidgetEvent::PointerDown { x, y, .. } => {
                // Check if click is in the tab row.
                let tab_height = 36;
                if *y < self.rect.y || *y >= self.rect.y + tab_height {
                    // Forward to active panel content.
                    if let Some(panel) = self.content.get_mut(self.active) {
                        return panel.event(event, phase, sink);
                    }
                    return EventResponse::Ignored;
                }

                // Determine which tab was clicked.
                let layout = self.tab_layout(self.rect.width);
                let rel_x = *x - self.rect.x;
                for (i, (tab_x, tab_w)) in layout.iter().enumerate() {
                    if rel_x >= *tab_x && rel_x < *tab_x + *tab_w {
                        if i != self.active {
                            self.active = i;
                            sink.emit(MessageId::with_payload(self.on_change.id, i as u32));
                        }
                        return EventResponse::Consumed;
                    }
                }
                EventResponse::Ignored
            }

            WidgetEvent::KeyDown { key, .. } => match key {
                Key::Named(NamedKey::Left) => {
                    if !self.tabs.is_empty() && self.active > 0 {
                        self.active -= 1;
                        sink.emit(MessageId::with_payload(
                            self.on_change.id,
                            self.active as u32,
                        ));
                        EventResponse::Consumed
                    } else {
                        EventResponse::Ignored
                    }
                }
                Key::Named(NamedKey::Right) => {
                    if !self.tabs.is_empty() && self.active + 1 < self.tabs.len() {
                        self.active += 1;
                        sink.emit(MessageId::with_payload(
                            self.on_change.id,
                            self.active as u32,
                        ));
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

            // Forward all other events (Scroll, PointerMove, etc.) to active panel.
            _ => {
                if let Some(panel) = self.content.get_mut(self.active) {
                    panel.event(event, phase, sink)
                } else {
                    EventResponse::Ignored
                }
            }
        }
    }

    fn role(&self) -> Role {
        Role::Tab
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
        &self.content
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.content
    }
}
