use slopos_abi::draw::Color32;

use crate::appkit::constraints::{BoxConstraints, Rect, Size};
use crate::appkit::event::{
    EventPhase, EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent,
};
use crate::appkit::node::MessageId;
use crate::appkit::paint::PaintContext;
use crate::appkit::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId, next_widget_id};

/// Modal dialog rendered as a centered card with semi-transparent backdrop.
///
/// The dialog fills its parent (for the backdrop) and centers a card containing
/// a title, content widget, and a row of action widgets (typically buttons).
pub struct DialogWidget {
    id: WidgetId,
    rect: Rect,
    title: String,
    content: Box<dyn Widget>,
    actions: Vec<Box<dyn Widget>>,
    on_dismiss: MessageId,
    /// Cached card rect from the last layout pass.
    card_rect: Rect,
}

impl DialogWidget {
    pub fn new(
        title: String,
        content: Box<dyn Widget>,
        actions: Vec<Box<dyn Widget>>,
        on_dismiss: MessageId,
    ) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            title,
            content,
            actions,
            on_dismiss,
            card_rect: Rect::ZERO,
        }
    }
}

/// Internal padding inside the card.
const CARD_PADDING: i32 = 16;
/// Spacing between action buttons.
const ACTION_SPACING: i32 = 8;

impl Widget for DialogWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let card_w = 300.min(constraints.max_width);
        let inner_w = (card_w - CARD_PADDING * 2).max(0);

        // Title row height.
        let title_h = crate::gfx::font::cell_height() + CARD_PADDING;

        // Measure content widget.
        let content_constraints = BoxConstraints {
            min_width: 0,
            max_width: inner_w,
            min_height: 0,
            max_height: i32::MAX,
        };
        let content_size = self.content.measure(content_constraints, ctx);

        // Measure action widgets and sum their widths.
        let mut actions_w = 0i32;
        let mut actions_h = 0i32;
        for (i, action) in self.actions.iter_mut().enumerate() {
            let action_size = action.measure(BoxConstraints::UNBOUNDED, ctx);
            actions_w += action_size.width;
            actions_h = actions_h.max(action_size.height);
            if i > 0 {
                actions_w += ACTION_SPACING;
            }
        }

        let _card_h = title_h + content_size.height + CARD_PADDING + actions_h + CARD_PADDING;
        let _ = actions_w; // used only during layout centering

        // Dialog fills parent for the backdrop overlay.
        constraints.constrain(constraints.max_size())
    }

    fn layout(&mut self, rect: Rect) {
        self.rect = rect;

        let card_w = 300.min(rect.width);
        let inner_w = (card_w - CARD_PADDING * 2).max(0);

        // Recompute heights for layout (matching measure).
        let title_h = crate::gfx::font::cell_height() + CARD_PADDING;

        let content_rect_y_offset = title_h;
        let content_layout_rect = Rect::new(0, 0, inner_w, i32::MAX);
        // We need content height; use the layout_rect from the content after a
        // pseudo-layout or rely on the measured size. Since measure was already
        // called, layout_rect won't be set yet. We'll lay out content first
        // with a temporary rect, read its height, then finalize positions.
        self.content.layout(content_layout_rect);
        let content_h = self.content.layout_rect().height.max(0);

        // Lay out each action with unbounded rect to discover natural sizes.
        let mut actions_total_w = 0i32;
        let mut actions_h = 0i32;
        let mut action_widths = Vec::with_capacity(self.actions.len());
        for action in self.actions.iter_mut() {
            action.layout(Rect::new(0, 0, i32::MAX, i32::MAX));
            let ar = action.layout_rect();
            action_widths.push(ar.width);
            actions_h = actions_h.max(ar.height);
        }
        for (i, w) in action_widths.iter().enumerate() {
            actions_total_w += *w;
            if i > 0 {
                actions_total_w += ACTION_SPACING;
            }
        }

        let card_h = title_h + content_h + CARD_PADDING + actions_h + CARD_PADDING;

        // Center card in parent rect.
        let card_x = rect.x + (rect.width - card_w) / 2;
        let card_y = rect.y + (rect.height - card_h) / 2;
        self.card_rect = Rect::new(card_x, card_y, card_w, card_h);

        // Layout content at its final position.
        let content_x = card_x + CARD_PADDING;
        let content_y = card_y + content_rect_y_offset;
        self.content
            .layout(Rect::new(content_x, content_y, inner_w, content_h));

        // Layout actions: centered horizontally at bottom of card.
        let actions_row_y = card_y + card_h - CARD_PADDING - actions_h;
        let mut ax = card_x + (card_w - actions_total_w) / 2;
        for (i, action) in self.actions.iter_mut().enumerate() {
            let aw = action_widths[i];
            action.layout(Rect::new(ax, actions_row_y, aw, actions_h));
            ax += aw + ACTION_SPACING;
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let style = ctx.style;

        // 1. Semi-transparent backdrop.
        ctx.fill_rect_blended(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            Color32::new(0, 0, 0, 128),
        );

        // 2. Card background.
        ctx.fill_rounded_rect(
            self.card_rect.x,
            self.card_rect.y,
            self.card_rect.width,
            self.card_rect.height,
            style.corner_radius,
            style.bg_secondary,
        );

        // 3. Card border.
        ctx.draw_rounded_rect(
            self.card_rect.x,
            self.card_rect.y,
            self.card_rect.width,
            self.card_rect.height,
            style.corner_radius,
            style.border_default,
        );

        // 4. Title text.
        let text_h = ctx.text_height();
        let title_x = self.card_rect.x + CARD_PADDING;
        let title_y =
            self.card_rect.y + (CARD_PADDING + crate::gfx::font::cell_height() - text_h) / 2;
        ctx.draw_text_transparent(title_x, title_y, &self.title, style.text_primary);

        // 5. Content widget.
        self.content.paint(ctx);

        // 6. Action widgets.
        for action in &self.actions {
            action.paint(ctx);
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
            // Escape dismisses the dialog.
            WidgetEvent::KeyDown {
                key: Key::Named(NamedKey::Escape),
                ..
            } => {
                sink.emit(self.on_dismiss);
                EventResponse::Consumed
            }

            // Click outside card dismisses the dialog.
            WidgetEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
            } => {
                if !self.card_rect.contains(*x, *y) {
                    sink.emit(self.on_dismiss);
                    return EventResponse::Consumed;
                }
                // Forward to action widgets.
                for action in &mut self.actions {
                    let resp = action.event(event, EventPhase::Target, sink);
                    if resp.is_consumed() {
                        return resp;
                    }
                }
                // Forward to content.
                self.content.event(event, EventPhase::Target, sink)
            }

            // Forward other events to children.
            _ => {
                for action in &mut self.actions {
                    let resp = action.event(event, phase, sink);
                    if resp.is_consumed() {
                        return resp;
                    }
                }
                self.content.event(event, phase, sink)
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
        // Cannot return a combined slice of content + actions without allocation.
        // Return actions; content is handled separately in paint/event.
        &self.actions
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.actions
    }
}
