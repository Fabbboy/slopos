use super::constraints::{BoxConstraints, Rect, Size};
use super::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use super::paint::PaintContext;
use super::style::StyleSheet;

/// Unique identifier for a widget instance in the retained tree.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct WidgetId(pub u32);

impl WidgetId {
    pub const NONE: Self = Self(0);
}

/// Counter for generating unique widget IDs.
static NEXT_WIDGET_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

pub fn next_widget_id() -> WidgetId {
    WidgetId(NEXT_WIDGET_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// Context passed during the measure phase.
pub struct MeasureCtx<'a> {
    pub style: &'a StyleSheet,
}

/// Identity and geometry every widget carries, owned by the framework.
///
/// `measured` is written only by [`measure_widget`] and `rect` only by
/// [`place_widget`], which is what makes "lay it out somewhere fake and read
/// the size back" unwritable: the size a parent needs is recorded during
/// measure and read with [`Widget::measured_size`], so no caller ever has a
/// reason to invent a rect.
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
/// The only sanctioned way to ask a child how big it wants to be. Containers
/// must call this rather than `Widget::measure` directly, so that
/// [`Widget::measured_size`] is always populated before layout runs.
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
/// The only sanctioned way to position a child. `rect` must be a real
/// destination: passing a sentinel to probe a size is precisely the bug this
/// split exists to prevent — use [`measure_widget`] and
/// [`Widget::measured_size`] instead.
pub fn place_widget(widget: &mut dyn Widget, rect: Rect) {
    widget.core_mut().rect = rect;
    widget.layout(rect);
}

/// Every widget implements this trait. The framework calls these methods
/// during each phase of the frame cycle.
pub trait Widget {
    /// Framework-owned identity and geometry. Implemented by storing a
    /// [`WidgetCore`] and returning references to it.
    fn core(&self) -> &WidgetCore;
    fn core_mut(&mut self) -> &mut WidgetCore;

    /// Compute this widget's desired size given parent constraints.
    ///
    /// Call [`measure_widget`] on children rather than this directly. May be
    /// called more than once per frame with different constraints, so it must
    /// stay free of side effects beyond caching.
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size;

    /// Position children within `rect`, the widget's final on-screen area.
    ///
    /// `rect` is already recorded by [`place_widget`]; implementations only
    /// need to place their own children, via [`place_widget`] in turn. A leaf
    /// widget needs no implementation at all.
    fn layout(&mut self, rect: Rect) {
        let _ = rect;
    }

    /// Paint this widget into the paint context.
    fn paint(&self, ctx: &mut PaintContext);

    /// Handle an input event. Return whether the event was consumed.
    /// Use `sink.emit(msg)` to send messages to the application.
    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse;

    /// Return this widget's accessibility role.
    fn role(&self) -> Role {
        Role::None
    }

    /// Return this widget's accessible name.
    fn accessible_name(&self) -> Option<&str> {
        None
    }

    /// Focus policy: can this widget receive keyboard focus?
    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }

    /// The unique ID of this widget instance.
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

    /// Access children for tree traversal.
    fn children(&self) -> &[Box<dyn Widget>] {
        &[]
    }

    /// Mutable access to children for tree traversal.
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut []
    }

    /// Flex weight for proportional space distribution in stacks.
    /// Return 0 (default) for non-flexible children.
    fn flex_weight(&self) -> u16 {
        0
    }

    /// Whether this widget needs a repaint.
    fn is_dirty(&self) -> bool {
        true
    }

    /// Mark this widget as needing a repaint.
    fn mark_dirty(&mut self) {}
}

/// Accessibility role (minimal set for v1).
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

/// Focus policy for a widget.
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
