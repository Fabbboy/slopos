use slopos_abi::draw::Color32;

use crate::ui::constraints::{BoxConstraints, CrossAxisAlignment, EdgeInsets, Length, Rect, Size};
use crate::ui::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::ui::paint::PaintContext;
use crate::ui::traits::{MeasureCtx, Widget, WidgetId, next_widget_id};

// ---------------------------------------------------------------------------
// Axis helpers
// ---------------------------------------------------------------------------

fn main_axis(size: Size, vertical: bool) -> i32 {
    if vertical { size.height } else { size.width }
}

fn cross_axis(size: Size, vertical: bool) -> i32 {
    if vertical { size.width } else { size.height }
}

fn child_constraints(bc: &BoxConstraints, vertical: bool) -> BoxConstraints {
    if vertical {
        BoxConstraints {
            min_width: bc.min_width,
            max_width: bc.max_width,
            min_height: 0,
            max_height: i32::MAX,
        }
    } else {
        BoxConstraints {
            min_width: 0,
            max_width: i32::MAX,
            min_height: bc.min_height,
            max_height: bc.max_height,
        }
    }
}

// ---------------------------------------------------------------------------
// StackWidget (internal, parameterized by axis)
// ---------------------------------------------------------------------------

struct StackWidget {
    id: WidgetId,
    rect: Rect,
    children: Vec<Box<dyn Widget>>,
    spacing: i32,
    cross_align: CrossAxisAlignment,
    vertical: bool,
    child_sizes: Vec<Size>,
}

impl StackWidget {
    fn new(
        children: Vec<Box<dyn Widget>>,
        spacing: i32,
        align: CrossAxisAlignment,
        vertical: bool,
    ) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            children,
            spacing,
            cross_align: align,
            vertical,
            child_sizes: Vec::new(),
        }
    }

    fn measure_impl(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let v = self.vertical;
        let loose = child_constraints(&constraints, v);
        let max_main = if v {
            constraints.max_height
        } else {
            constraints.max_width
        };
        self.child_sizes.clear();
        self.child_sizes.resize(self.children.len(), Size::ZERO);

        let mut total_fixed: i32 = 0;
        let mut max_cross: i32 = 0;
        let mut total_flex_weight: u32 = 0;

        // Pass 1: Measure non-flex children, tally flex weights.
        for (i, child) in self.children.iter_mut().enumerate() {
            let w = child.flex_weight();
            if w > 0 {
                total_flex_weight += w as u32;
            } else {
                let child_size = child.measure(loose, ctx);
                total_fixed = total_fixed.saturating_add(main_axis(child_size, v));
                max_cross = max_cross.max(cross_axis(child_size, v));
                self.child_sizes[i] = child_size;
            }
        }

        let gap_count = if self.children.len() > 1 {
            (self.children.len() - 1) as i32
        } else {
            0
        };
        let spacing_total = gap_count.saturating_mul(self.spacing);

        // Pass 2: Distribute remaining space to flex children.
        let mut total_main = total_fixed;
        if total_flex_weight > 0 && max_main < i32::MAX {
            let remaining = (max_main - total_fixed - spacing_total).max(0);
            for (i, child) in self.children.iter_mut().enumerate() {
                let w = child.flex_weight();
                if w > 0 {
                    let share = (remaining as u32 * w as u32 / total_flex_weight) as i32;
                    let tight = if v {
                        BoxConstraints {
                            min_width: loose.min_width,
                            max_width: loose.max_width,
                            min_height: share,
                            max_height: share,
                        }
                    } else {
                        BoxConstraints {
                            min_width: share,
                            max_width: share,
                            min_height: loose.min_height,
                            max_height: loose.max_height,
                        }
                    };
                    let child_size = child.measure(tight, ctx);
                    max_cross = max_cross.max(cross_axis(child_size, v));
                    self.child_sizes[i] = child_size;
                    total_main = total_main.saturating_add(main_axis(child_size, v));
                }
            }
        } else if total_flex_weight > 0 {
            // Unbounded main axis: measure flex children with loose constraints.
            for (i, child) in self.children.iter_mut().enumerate() {
                if child.flex_weight() > 0 {
                    let child_size = child.measure(loose, ctx);
                    max_cross = max_cross.max(cross_axis(child_size, v));
                    self.child_sizes[i] = child_size;
                    total_main = total_main.saturating_add(main_axis(child_size, v));
                }
            }
        }

        total_main = total_main.saturating_add(spacing_total);

        let size = if v {
            Size::new(max_cross, total_main)
        } else {
            Size::new(total_main, max_cross)
        };
        constraints.constrain(size)
    }

    fn layout_impl(&mut self, rect: Rect) {
        self.rect = rect;
        let v = self.vertical;
        let avail_main = if v { rect.height } else { rect.width };
        let avail_cross = cross_axis(Size::new(rect.width, rect.height), v);

        // Recompute flex shares for layout (rect may differ from measure).
        let mut total_fixed: i32 = 0;
        let mut total_flex_weight: u32 = 0;
        let n = self.child_sizes.len();
        for i in 0..self.children.len().min(n) {
            let w = self.children[i].flex_weight();
            if w > 0 {
                total_flex_weight += w as u32;
            } else {
                total_fixed = total_fixed.saturating_add(main_axis(self.child_sizes[i], v));
            }
        }

        let gap_count = if self.children.len() > 1 {
            (self.children.len() - 1) as i32
        } else {
            0
        };
        let spacing_total = gap_count.saturating_mul(self.spacing);
        let remaining = (avail_main - total_fixed - spacing_total).max(0);

        let mut cursor: i32 = 0;
        for i in 0..self.children.len().min(n) {
            let child_main = if total_flex_weight > 0 && self.children[i].flex_weight() > 0 {
                let w = self.children[i].flex_weight() as u32;
                (remaining as u32 * w / total_flex_weight) as i32
            } else {
                main_axis(self.child_sizes[i], v)
            };
            let child_cross = cross_axis(self.child_sizes[i], v);

            let cross_pos = match self.cross_align {
                CrossAxisAlignment::Start => 0,
                CrossAxisAlignment::Center => (avail_cross - child_cross) / 2,
                CrossAxisAlignment::End => avail_cross - child_cross,
                CrossAxisAlignment::Stretch => 0,
            };
            let layout_cross = if self.cross_align == CrossAxisAlignment::Stretch {
                avail_cross
            } else {
                child_cross
            };

            let (abs_x, abs_y, w, h) = if v {
                (
                    rect.x + cross_pos,
                    rect.y + cursor,
                    layout_cross,
                    child_main,
                )
            } else {
                (
                    rect.x + cursor,
                    rect.y + cross_pos,
                    child_main,
                    layout_cross,
                )
            };
            self.children[i].layout(Rect::new(abs_x, abs_y, w, h));
            cursor += child_main + self.spacing;
        }
    }

    fn paint_impl(&self, ctx: &mut PaintContext) {
        for child in &self.children {
            child.paint(ctx);
        }
    }

    fn event_impl(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        // For pointer events, only forward to children whose rect contains
        // the pointer position. This prevents a Table from stealing clicks
        // meant for a sidebar ListView in the same HStack.
        let pointer_pos = match event {
            WidgetEvent::PointerDown { x, y, .. }
            | WidgetEvent::PointerUp { x, y, .. }
            | WidgetEvent::PointerMove { x, y } => Some((*x, *y)),
            _ => None,
        };

        for child in self.children.iter_mut().rev() {
            if let Some((px, py)) = pointer_pos {
                let r = child.layout_rect();
                if !r.contains(px, py) {
                    continue;
                }
            }
            let resp = child.event(event, phase, sink);
            if resp.is_consumed() {
                return resp;
            }
        }
        EventResponse::Ignored
    }
}

// ---------------------------------------------------------------------------
// VStackWidget
// ---------------------------------------------------------------------------

pub struct VStackWidget {
    inner: StackWidget,
}

impl VStackWidget {
    pub fn new(children: Vec<Box<dyn Widget>>, spacing: i32, align: CrossAxisAlignment) -> Self {
        Self {
            inner: StackWidget::new(children, spacing, align, true),
        }
    }
}

impl Widget for VStackWidget {
    fn measure(&mut self, c: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.inner.measure_impl(c, ctx)
    }
    fn layout(&mut self, r: Rect) {
        self.inner.layout_impl(r);
    }
    fn paint(&self, ctx: &mut PaintContext) {
        self.inner.paint_impl(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, p: EventPhase, sink: &mut MessageSink) -> EventResponse {
        self.inner.event_impl(e, p, sink)
    }
    fn id(&self) -> WidgetId {
        self.inner.id
    }
    fn layout_rect(&self) -> Rect {
        self.inner.rect
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        &self.inner.children
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.inner.children
    }
}

// ---------------------------------------------------------------------------
// HStackWidget
// ---------------------------------------------------------------------------

pub struct HStackWidget {
    inner: StackWidget,
}

impl HStackWidget {
    pub fn new(children: Vec<Box<dyn Widget>>, spacing: i32, align: CrossAxisAlignment) -> Self {
        Self {
            inner: StackWidget::new(children, spacing, align, false),
        }
    }
}

impl Widget for HStackWidget {
    fn measure(&mut self, c: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.inner.measure_impl(c, ctx)
    }
    fn layout(&mut self, r: Rect) {
        self.inner.layout_impl(r);
    }
    fn paint(&self, ctx: &mut PaintContext) {
        self.inner.paint_impl(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, p: EventPhase, sink: &mut MessageSink) -> EventResponse {
        self.inner.event_impl(e, p, sink)
    }
    fn id(&self) -> WidgetId {
        self.inner.id
    }
    fn layout_rect(&self) -> Rect {
        self.inner.rect
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        &self.inner.children
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.inner.children
    }
}

// ---------------------------------------------------------------------------
// ZStackWidget
// ---------------------------------------------------------------------------

pub struct ZStackWidget {
    id: WidgetId,
    rect: Rect,
    children: Vec<Box<dyn Widget>>,
}

impl ZStackWidget {
    pub fn new(children: Vec<Box<dyn Widget>>) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            children,
        }
    }
}

impl Widget for ZStackWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let loose = constraints.loosen();
        let mut max_w: i32 = 0;
        let mut max_h: i32 = 0;
        for child in &mut self.children {
            let s = child.measure(loose, ctx);
            max_w = max_w.max(s.width);
            max_h = max_h.max(s.height);
        }
        constraints.constrain(Size::new(max_w, max_h))
    }
    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
        for child in &mut self.children {
            child.layout(rect);
        }
    }
    fn paint(&self, ctx: &mut PaintContext) {
        for child in &self.children {
            child.paint(ctx);
        }
    }
    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        for child in self.children.iter_mut().rev() {
            let resp = child.event(event, phase, sink);
            if resp.is_consumed() {
                return resp;
            }
        }
        EventResponse::Ignored
    }
    fn id(&self) -> WidgetId {
        self.id
    }
    fn layout_rect(&self) -> Rect {
        self.rect
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        &self.children
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.children
    }
}

// ---------------------------------------------------------------------------
// PaddingWidget
// ---------------------------------------------------------------------------

pub struct PaddingWidget {
    id: WidgetId,
    rect: Rect,
    insets: EdgeInsets,
    child: Box<dyn Widget>,
}

impl PaddingWidget {
    pub fn new(insets: EdgeInsets, child: Box<dyn Widget>) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            insets,
            child,
        }
    }
}

impl Widget for PaddingWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let inner = constraints.deflate(self.insets);
        let child_size = self.child.measure(inner, ctx);
        constraints.constrain(Size::new(
            child_size.width + self.insets.horizontal(),
            child_size.height + self.insets.vertical(),
        ))
    }
    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
        self.child.layout(Rect::new(
            rect.x + self.insets.left,
            rect.y + self.insets.top,
            (rect.width - self.insets.horizontal()).max(0),
            (rect.height - self.insets.vertical()).max(0),
        ));
    }
    fn paint(&self, ctx: &mut PaintContext) {
        self.child.paint(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, p: EventPhase, sink: &mut MessageSink) -> EventResponse {
        self.child.event(e, p, sink)
    }
    fn id(&self) -> WidgetId {
        self.id
    }
    fn layout_rect(&self) -> Rect {
        self.rect
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}

// ---------------------------------------------------------------------------
// SpacerWidget
// ---------------------------------------------------------------------------

pub struct SpacerWidget {
    id: WidgetId,
    rect: Rect,
    length: Length,
}

impl SpacerWidget {
    pub fn new(length: Length) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            length,
        }
    }
}

impl Widget for SpacerWidget {
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let px = match self.length {
            Length::Px(n) => n,
            Length::Fill(_) => 0, // fill handled by parent flex
        };
        constraints.constrain(Size::new(px, px))
    }
    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
    }
    fn paint(&self, _ctx: &mut PaintContext) {}
    fn event(
        &mut self,
        _e: &WidgetEvent,
        _p: EventPhase,
        _sink: &mut MessageSink,
    ) -> EventResponse {
        EventResponse::Ignored
    }
    fn id(&self) -> WidgetId {
        self.id
    }
    fn layout_rect(&self) -> Rect {
        self.rect
    }
}

// ---------------------------------------------------------------------------
// ExpandWidget
// ---------------------------------------------------------------------------

pub struct ExpandWidget {
    id: WidgetId,
    rect: Rect,
    weight: u16,
    child: Box<dyn Widget>,
}

impl ExpandWidget {
    pub fn new(weight: u16, child: Box<dyn Widget>) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            weight,
            child,
        }
    }
}

impl Widget for ExpandWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let child_size = self.child.measure(constraints, ctx);
        Size::new(
            child_size.width.max(constraints.min_width),
            child_size.height.max(constraints.min_height),
        )
    }
    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
        self.child.layout(rect);
    }
    fn paint(&self, ctx: &mut PaintContext) {
        self.child.paint(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, p: EventPhase, sink: &mut MessageSink) -> EventResponse {
        self.child.event(e, p, sink)
    }
    fn flex_weight(&self) -> u16 {
        self.weight
    }
    fn id(&self) -> WidgetId {
        self.id
    }
    fn layout_rect(&self) -> Rect {
        self.rect
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}

// ---------------------------------------------------------------------------
// BackgroundWidget
// ---------------------------------------------------------------------------

pub struct BackgroundWidget {
    id: WidgetId,
    rect: Rect,
    color: Color32,
    child: Box<dyn Widget>,
}

impl BackgroundWidget {
    pub fn new(color: Color32, child: Box<dyn Widget>) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            color,
            child,
        }
    }
}

impl Widget for BackgroundWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.child.measure(constraints, ctx)
    }
    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
        self.child.layout(rect);
    }
    fn paint(&self, ctx: &mut PaintContext) {
        ctx.fill_rect(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            self.color,
        );
        self.child.paint(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, p: EventPhase, sink: &mut MessageSink) -> EventResponse {
        self.child.event(e, p, sink)
    }
    fn id(&self) -> WidgetId {
        self.id
    }
    fn layout_rect(&self) -> Rect {
        self.rect
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}

// ---------------------------------------------------------------------------
// SizedBoxWidget
// ---------------------------------------------------------------------------

pub struct SizedBoxWidget {
    id: WidgetId,
    rect: Rect,
    width: Option<Length>,
    height: Option<Length>,
    child: Box<dyn Widget>,
}

impl SizedBoxWidget {
    pub fn new(width: Option<Length>, height: Option<Length>, child: Box<dyn Widget>) -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            width,
            height,
            child,
        }
    }

    fn resolve_w(&self, constraints: &BoxConstraints) -> Option<i32> {
        self.width
            .map(|l| l.resolve(constraints.min_width, constraints.max_width))
    }

    fn resolve_h(&self, constraints: &BoxConstraints) -> Option<i32> {
        self.height
            .map(|l| l.resolve(constraints.min_height, constraints.max_height))
    }
}

impl Widget for SizedBoxWidget {
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let rw = self.resolve_w(&constraints);
        let rh = self.resolve_h(&constraints);
        let inner = BoxConstraints {
            min_width: rw.unwrap_or(constraints.min_width),
            max_width: rw.unwrap_or(constraints.max_width),
            min_height: rh.unwrap_or(constraints.min_height),
            max_height: rh.unwrap_or(constraints.max_height),
        };
        let child_size = self.child.measure(inner, ctx);
        Size::new(
            rw.unwrap_or(child_size.width),
            rh.unwrap_or(child_size.height),
        )
    }
    fn layout(&mut self, rect: Rect) {
        self.rect = rect;
        self.child.layout(rect);
    }
    fn paint(&self, ctx: &mut PaintContext) {
        self.child.paint(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, p: EventPhase, sink: &mut MessageSink) -> EventResponse {
        self.child.event(e, p, sink)
    }
    fn id(&self) -> WidgetId {
        self.id
    }
    fn layout_rect(&self) -> Rect {
        self.rect
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}
