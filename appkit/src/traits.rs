use super::constraints::{BoxConstraints, Rect, Size};
use super::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use super::paint::PaintContext;
use super::style::StyleSheet;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct WidgetId(pub u32);

impl WidgetId {
    pub const NONE: Self = Self(0);
}

static NEXT_WIDGET_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

pub fn next_widget_id() -> WidgetId {
    WidgetId(NEXT_WIDGET_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

pub struct MeasureCtx<'a> {
    pub style: &'a StyleSheet,
}

/// Identity and geometry every widget carries, owned by the framework.
///
/// `measured` is written only by [`measure_widget`] and `rect` only by
/// [`place_widget`], so laying a widget out at a fake rect to read its size back
/// is unwritable.
#[derive(Debug)]
pub struct WidgetCore {
    id: WidgetId,
    rect: Rect,
    measured: Size,
}

impl WidgetCore {
    pub fn new() -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            measured: Size::ZERO,
        }
    }

    pub fn id(&self) -> WidgetId {
        self.id
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    pub fn measured(&self) -> Size {
        self.measured
    }
}

impl Default for WidgetCore {
    fn default() -> Self {
        Self::new()
    }
}

/// Measure `widget` under `constraints` and record the result.
///
/// Containers must call this rather than `Widget::measure` directly, so that
/// [`Widget::measured_size`] is populated before layout runs.
pub fn measure_widget(
    widget: &mut dyn Widget,
    constraints: BoxConstraints,
    ctx: &mut MeasureCtx,
) -> Size {
    let size = widget.measure(constraints, ctx);
    let size = constraints.constrain(size);
    widget.core_mut().measured = size;
    size
}

/// Assign `widget` its final on-screen rect.
///
/// `rect` must be a real destination — to probe a size, use [`measure_widget`]
/// and [`Widget::measured_size`] instead.
pub fn place_widget(widget: &mut dyn Widget, rect: Rect) {
    widget.core_mut().rect = rect;
    widget.layout(rect);
}

pub trait Widget {
    /// Implemented by storing a [`WidgetCore`] and returning references to it.
    fn core(&self) -> &WidgetCore;
    fn core_mut(&mut self) -> &mut WidgetCore;

    /// Compute this widget's desired size given parent constraints.
    ///
    /// May be called more than once per frame with different constraints, so it
    /// must stay free of side effects beyond caching.
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size;

    /// Position children within `rect`, the widget's final on-screen area.
    ///
    /// `rect` is already recorded by [`place_widget`]; a leaf widget needs no
    /// implementation at all.
    fn layout(&mut self, rect: Rect) {
        let _ = rect;
    }

    fn paint(&self, ctx: &mut PaintContext);

    /// Use `sink.emit(msg)` to send messages to the application.
    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse;

    fn role(&self) -> Role {
        Role::None
    }

    fn accessible_name(&self) -> Option<&str> {
        None
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }

    fn id(&self) -> WidgetId {
        self.core().id()
    }

    /// The layout rect assigned during the last layout pass.
    fn layout_rect(&self) -> Rect {
        self.core().rect()
    }

    /// The size recorded by the last [`measure_widget`] call.
    fn measured_size(&self) -> Size {
        self.core().measured()
    }

    fn children(&self) -> &[Box<dyn Widget>] {
        &[]
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut []
    }

    /// Proportional share of leftover stack space; 0 means non-flexible.
    fn flex_weight(&self) -> u16 {
        0
    }

    fn is_dirty(&self) -> bool {
        true
    }

    fn mark_dirty(&mut self) {}
}

/// Accessibility role.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Role {
    None,
    Button,
    TextField,
    Checkbox,
    Label,
    List,
    ListItem,
    ScrollArea,
    Tab,
    TabPanel,
    Menu,
    MenuItem,
    Separator,
    Group,
    Window,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum FocusPolicy {
    /// Never focusable.
    #[default]
    None,
    /// Focusable via Tab key.
    TabFocus,
    /// Focusable via mouse click only.
    ClickFocus,
    /// Focusable via both Tab and click.
    StrongFocus,
}

impl FocusPolicy {
    pub fn is_focusable(&self) -> bool {
        !matches!(self, FocusPolicy::None)
    }

    pub fn is_tab_focusable(&self) -> bool {
        matches!(self, FocusPolicy::TabFocus | FocusPolicy::StrongFocus)
    }
}
