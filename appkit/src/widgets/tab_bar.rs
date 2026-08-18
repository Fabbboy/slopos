use std::any::Any;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventPhase, EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{
    FocusPolicy, MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget,
};

/// A row of tab labels above the active panel's content.
pub struct TabBarWidget {
    core: WidgetCore,
    tabs: Vec<String>,
    active: usize,
    on_change: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    content: Vec<Box<dyn Widget>>,
    focused: bool,
    /// Tab strip height from the last measure; layout and hit testing must read
    /// it rather than re-derive the constant.
    tab_height: i32,
}

impl TabBarWidget {
    pub fn new(
        tabs: Vec<String>,
        active: usize,
        on_change: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
        content: Vec<Box<dyn Widget>>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            tabs,
            active,
            on_change,
            content,
            focused: false,
            tab_height: 0,
        }
    }

    fn panel_rect(&self) -> Rect {
        let rect = self.layout_rect();
        Rect::new(
            rect.x,
            rect.y + self.tab_height,
            rect.width,
            (rect.height - self.tab_height).max(0),
        )
    }

    /// Returns `(x_start, width)` per tab.
    fn tab_layout(&self, total_width: i32) -> Vec<(i32, i32)> {
        let count = self.tabs.len();
        if count == 0 {
            return Vec::new();
        }
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
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let tab_height = ctx.style.tab_height;
        self.tab_height = tab_height;
        let w = constraints.max_width;

        let panel_constraints = BoxConstraints {
            min_width: w,
            max_width: w,
            min_height: 0,
            max_height: if constraints.is_height_bounded() {
                (constraints.max_height - tab_height).max(0)
            } else {
                crate::constraints::MAX_EXTENT
            },
        };

        // Measure every panel so a tab switch needs no fresh measure pass.
        let mut panel_height = 0;
        for (i, panel) in self.content.iter_mut().enumerate() {
            let size = measure_widget(panel.as_mut(), panel_constraints, ctx);
            if i == self.active {
                panel_height = size.height;
            }
        }

        constraints.constrain(Size::new(w, tab_height + panel_height))
    }

    fn layout(&mut self, _rect: Rect) {
        let panel_rect = self.panel_rect();
        for panel in &mut self.content {
            place_widget(panel.as_mut(), panel_rect);
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let tab_height = self.tab_height;
        let underline_height = 3;

        ctx.fill_rect(
            rect.x,
            rect.y,
            rect.width,
            tab_height,
            ctx.style.bg_secondary,
        );

        let layout = self.tab_layout(rect.width);
        let text_h = ctx.text_height();

        for (i, (tab_x, tab_w)) in layout.iter().enumerate() {
            let abs_x = rect.x + tab_x;

            if i == self.active {
                ctx.fill_rect(abs_x, rect.y, *tab_w, tab_height, ctx.style.bg_primary);

                ctx.fill_rect(
                    abs_x,
                    rect.y + tab_height - underline_height,
                    *tab_w,
                    underline_height,
                    ctx.style.bg_accent,
                );
            }

            let label = &self.tabs[i];
            let text_w = ctx.text_width(label);
            let tx = abs_x + (*tab_w - text_w) / 2;
            let ty = rect.y + (tab_height - text_h) / 2;

            let fg = if i == self.active {
                ctx.style.text_primary
            } else {
                ctx.style.text_secondary
            };
            ctx.draw_text_transparent(tx, ty, label, fg);
        }

        ctx.fill_rect(
            rect.x,
            rect.y + tab_height,
            rect.width,
            1,
            ctx.style.border_divider,
        );

        // A panel taller than the area left for it must not draw over the tabs.
        if let Some(panel) = self.content.get(self.active) {
            let panel_rect = self.panel_rect();
            ctx.with_clip(panel_rect, |ctx| panel.paint(ctx));
        }

        if self.focused {
            let tab_row = Rect::new(rect.x, rect.y, rect.width, tab_height);
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
                let rect = self.layout_rect();
                if *y < rect.y || *y >= rect.y + self.tab_height {
                    if let Some(panel) = self.content.get_mut(self.active) {
                        return panel.event(event, phase, sink);
                    }
                    return EventResponse::Ignored;
                }

                let layout = self.tab_layout(rect.width);
                let rel_x = *x - rect.x;
                for (i, (tab_x, tab_w)) in layout.iter().enumerate() {
                    if rel_x >= *tab_x && rel_x < *tab_x + *tab_w {
                        if i != self.active {
                            self.active = i;
                            if let Some(cb) = &self.on_change {
                                sink.emit_raw(cb(i));
                            }
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
                        if let Some(cb) = &self.on_change {
                            sink.emit_raw(cb(self.active));
                        }
                        EventResponse::Consumed
                    } else {
                        EventResponse::Ignored
                    }
                }
                Key::Named(NamedKey::Right) => {
                    if !self.tabs.is_empty() && self.active + 1 < self.tabs.len() {
                        self.active += 1;
                        if let Some(cb) = &self.on_change {
                            sink.emit_raw(cb(self.active));
                        }
                        EventResponse::Consumed
                    } else {
                        EventResponse::Ignored
                    }
                }
                _ => {
                    if let Some(panel) = self.content.get_mut(self.active) {
                        panel.event(event, phase, sink)
                    } else {
                        EventResponse::Ignored
                    }
                }
            },

            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                EventResponse::Ignored
            }

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

    fn children(&self) -> &[Box<dyn Widget>] {
        &self.content
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.content
    }
}
