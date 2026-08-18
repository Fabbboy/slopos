use slopos_abi::draw::Color32;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent,
};
use crate::paint::PaintContext;
use crate::traits::{
    FocusPolicy, MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget,
};

/// A centered card over a dimming backdrop. The widget fills its parent, so the
/// backdrop covers everything and no click reaches the tree underneath.
pub struct DialogWidget {
    core: WidgetCore,
    title: String,
    content: Box<dyn Widget>,
    actions: Vec<Box<dyn Widget>>,
    on_dismiss: Option<Box<dyn Fn() -> Box<dyn std::any::Any>>>,
    card_rect: Rect,
    /// `None` until Tab or an arrow key names one: a confirm dialog's first
    /// action is typically the destructive one, and a stray Enter must not fire
    /// the thing the dialog exists to ask about.
    focused_action: Option<usize>,
}

impl DialogWidget {
    pub fn new(
        title: String,
        content: Box<dyn Widget>,
        actions: Vec<Box<dyn Widget>>,
        on_dismiss: Option<Box<dyn Fn() -> Box<dyn std::any::Any>>>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            title,
            content,
            actions,
            on_dismiss,
            card_rect: Rect::ZERO,
            focused_action: None,
        }
    }

    /// Move the keyboard selection between actions, wrapping.
    fn cycle_action(&mut self, forward: bool) {
        let len = self.actions.len();
        if len == 0 {
            return;
        }
        self.focused_action = Some(match self.focused_action {
            Some(i) if forward => (i + 1) % len,
            Some(i) => (i + len - 1) % len,
            None if forward => 0,
            None => len - 1,
        });
    }

    /// The centered card, as positioned by the last layout pass.
    pub fn card_rect(&self) -> Rect {
        self.card_rect
    }

    fn card_width(&self, available: i32) -> i32 {
        CARD_MAX_WIDTH.min(available)
    }

    /// Width available to the content and the title inside the card.
    fn inner_width(&self, available: i32) -> i32 {
        (self.card_width(available) - CARD_PADDING * 2).max(0)
    }

    /// Action row width, gaps included.
    fn actions_width(&self) -> i32 {
        let mut total = 0;
        for (i, action) in self.actions.iter().enumerate() {
            if i > 0 {
                total += ACTION_SPACING;
            }
            total += action.measured_size().width;
        }
        total
    }

    /// Tallest action, which is the height of the action row.
    fn actions_height(&self) -> i32 {
        self.actions
            .iter()
            .map(|a| a.measured_size().height)
            .max()
            .unwrap_or(0)
    }

    /// Card height for the sizes recorded by the last measure pass.
    fn card_height(&self) -> i32 {
        let title_h = crate::text::cell_height() + CARD_PADDING;
        let content_h = self.content.measured_size().height;
        let actions_h = self.actions_height();
        let mut h = title_h + content_h + CARD_PADDING;
        if actions_h > 0 {
            h += actions_h + CARD_PADDING;
        }
        h
    }
}

const CARD_PADDING: i32 = 16;
const ACTION_SPACING: i32 = 8;
/// Preferred card width; narrower only when the window is.
const CARD_MAX_WIDTH: i32 = 300;

impl Widget for DialogWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let inner_w = self.inner_width(constraints.max_width);

        // Content wraps to the card's inner width and is as tall as it likes:
        // `card_height` reads that back rather than assuming a line count.
        let content_constraints = BoxConstraints {
            min_width: inner_w,
            max_width: inner_w,
            min_height: 0,
            max_height: crate::constraints::MAX_EXTENT,
        };
        measure_widget(self.content.as_mut(), content_constraints, ctx);

        let action_constraints = BoxConstraints::loose(Size::new(inner_w, constraints.max_height));
        for action in &mut self.actions {
            measure_widget(action.as_mut(), action_constraints, ctx);
        }

        // The dialog itself covers the parent so the backdrop dims everything.
        constraints.constrain(constraints.max_size())
    }

    fn layout(&mut self, rect: Rect) {
        let card_w = self.card_width(rect.width);
        let inner_w = self.inner_width(rect.width);
        let card_h = self.card_height().min(rect.height);

        let card_x = rect.x + (rect.width - card_w) / 2;
        let card_y = rect.y + (rect.height - card_h) / 2;
        self.card_rect = Rect::new(card_x, card_y, card_w, card_h);

        let title_h = crate::text::cell_height() + CARD_PADDING;
        let content_h = self.content.measured_size().height;
        place_widget(
            self.content.as_mut(),
            Rect::new(card_x + CARD_PADDING, card_y + title_h, inner_w, content_h),
        );

        let actions_h = self.actions_height();
        let actions_row_y = card_y + card_h - CARD_PADDING - actions_h;
        let mut ax = card_x + (card_w - self.actions_width()) / 2;
        for action in &mut self.actions {
            let aw = action.measured_size().width;
            place_widget(action.as_mut(), Rect::new(ax, actions_row_y, aw, actions_h));
            ax += aw + ACTION_SPACING;
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let style = ctx.style;
        let rect = self.layout_rect();

        ctx.fill_rect_blended(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            Color32::new(0, 0, 0, 128),
        );

        ctx.fill_rounded_rect(
            self.card_rect.x,
            self.card_rect.y,
            self.card_rect.width,
            self.card_rect.height,
            style.corner_radius,
            style.bg_secondary,
        );
        ctx.draw_rounded_rect(
            self.card_rect.x,
            self.card_rect.y,
            self.card_rect.width,
            self.card_rect.height,
            style.corner_radius,
            style.border_default,
        );

        let text_h = ctx.text_height();
        let title_x = self.card_rect.x + CARD_PADDING;
        let title_y = self.card_rect.y + (CARD_PADDING + crate::text::cell_height() - text_h) / 2;
        ctx.draw_text_transparent(title_x, title_y, &self.title, style.text_primary);

        let selected = self.focused_action;
        ctx.with_clip(self.card_rect, |ctx| {
            self.content.paint(ctx);
            for (i, action) in self.actions.iter().enumerate() {
                action.paint(ctx);
                if selected == Some(i) {
                    let r = action.layout_rect();
                    ctx.draw_rounded_rect(
                        r.x - 2,
                        r.y - 2,
                        r.width + 4,
                        r.height + 4,
                        style.corner_radius,
                        style.focus_ring_color,
                    );
                }
            }
        });
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
            WidgetEvent::KeyDown {
                key: Key::Named(NamedKey::Escape),
                ..
            } => {
                if let Some(f) = &self.on_dismiss {
                    sink.emit_raw(f());
                }
                EventResponse::Consumed
            }

            WidgetEvent::PointerDown { x, y, button } | WidgetEvent::PointerUp { x, y, button } => {
                if *button == PointerButton::Left
                    && matches!(event, WidgetEvent::PointerDown { .. })
                    && !self.card_rect.contains(*x, *y)
                {
                    if let Some(f) = &self.on_dismiss {
                        sink.emit_raw(f());
                    }
                    return EventResponse::Consumed;
                }

                // Only the action under the pointer gets the event; offering it
                // to each in turn would let "Cancel" fire "Kill".
                for action in &mut self.actions {
                    if action.layout_rect().contains(*x, *y) {
                        let resp = action.event(event, EventPhase::Target, sink);
                        if resp.is_consumed() {
                            return resp;
                        }
                    }
                }
                if self.content.layout_rect().contains(*x, *y) {
                    let resp = self.content.event(event, EventPhase::Target, sink);
                    if resp.is_consumed() {
                        return resp;
                    }
                }
                // Modal: a press inside the card that hit nothing must not fall
                // through to the tree the backdrop is covering.
                EventResponse::Consumed
            }

            WidgetEvent::KeyDown { key, modifiers, .. } => {
                match key {
                    Key::Named(NamedKey::Tab) => {
                        self.cycle_action(!modifiers.shift);
                        return EventResponse::Consumed;
                    }
                    Key::Named(NamedKey::Right) | Key::Named(NamedKey::Down) => {
                        self.cycle_action(true);
                        return EventResponse::Consumed;
                    }
                    Key::Named(NamedKey::Left) | Key::Named(NamedKey::Up) => {
                        self.cycle_action(false);
                        return EventResponse::Consumed;
                    }
                    _ => {}
                }

                // Only the selected action sees the key, so Enter cannot fire
                // whichever action happens to be listed first.
                if let Some(action) = self.focused_action.and_then(|i| self.actions.get_mut(i)) {
                    let resp = action.event(event, EventPhase::Target, sink);
                    if resp.is_consumed() {
                        return resp;
                    }
                }
                let resp = self.content.event(event, phase, sink);
                if resp.is_consumed() {
                    resp
                } else {
                    // Modal: swallow, so the app underneath does not act on a
                    // key aimed at the dialog.
                    EventResponse::Consumed
                }
            }

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

    fn children(&self) -> &[Box<dyn Widget>] {
        // Content is painted and hit-tested directly; only the actions need to
        // appear in the tab chain.
        &self.actions
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.actions
    }
}
