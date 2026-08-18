use super::constraints::{
    BoxConstraints, CrossAxisAlignment, EdgeInsets, Length, MAX_EXTENT, Rect, Size,
};
use super::event::{EventPhase, EventResponse, MessageSink, WidgetEvent, hit_test};
use super::focus::FocusManager;
use super::layout::{HStackWidget, PaddingWidget, SpacerWidget, VStackWidget};
use super::paint::PaintContext;
use super::style::StyleSheet;
use super::traits::{FocusPolicy, MeasureCtx, Widget, WidgetCore, measure_widget, place_widget};

/// Fixed measure size, so layout tests need no font.
struct FixedSizeWidget {
    core: WidgetCore,
    size: Size,
    focus: FocusPolicy,
}

impl FixedSizeWidget {
    fn new(width: i32, height: i32) -> Self {
        Self {
            core: WidgetCore::new(),
            size: Size::new(width, height),
            focus: FocusPolicy::None,
        }
    }

    fn focusable(width: i32, height: i32) -> Self {
        Self {
            core: WidgetCore::new(),
            size: Size::new(width, height),
            focus: FocusPolicy::StrongFocus,
        }
    }
}

impl Widget for FixedSizeWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        constraints.constrain(self.size)
    }
    fn paint(&self, _ctx: &mut PaintContext) {}
    fn event(
        &mut self,
        _event: &WidgetEvent,
        _phase: EventPhase,
        _sink: &mut MessageSink,
    ) -> EventResponse {
        EventResponse::Ignored
    }
    fn focus_policy(&self) -> FocusPolicy {
        self.focus
    }
}

/// A button stand-in that records which one was pressed.
struct ProbeButton {
    core: WidgetCore,
    name: &'static str,
    size: Size,
}

impl ProbeButton {
    fn new(name: &'static str, width: i32, height: i32) -> Self {
        Self {
            core: WidgetCore::new(),
            name,
            size: Size::new(width, height),
        }
    }
}

impl Widget for ProbeButton {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        constraints.constrain(self.size)
    }
    fn paint(&self, _ctx: &mut PaintContext) {}
    fn event(
        &mut self,
        event: &WidgetEvent,
        _phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        let activated = match event {
            WidgetEvent::PointerDown { .. } => true,
            WidgetEvent::KeyDown { key, .. } => matches!(
                key,
                super::event::Key::Named(super::event::NamedKey::Enter)
                    | super::event::Key::Named(super::event::NamedKey::Space)
            ),
            _ => false,
        };
        if activated {
            sink.emit_raw(Box::new(String::from(self.name)) as Box<dyn std::any::Any>);
            EventResponse::Consumed
        } else {
            EventResponse::Ignored
        }
    }
    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::StrongFocus
    }
}

fn test_tight_constraints() {
    let c = BoxConstraints::tight(Size::new(100, 50));
    assert!(c.is_tight());
    assert_eq!(c.min_width, 100);
    assert_eq!(c.max_width, 100);
    assert_eq!(c.min_height, 50);
    assert_eq!(c.max_height, 50);
    assert_eq!(c.constrain(Size::new(200, 200)), Size::new(100, 50));
    assert_eq!(c.constrain(Size::new(10, 5)), Size::new(100, 50));
}

fn test_loose_constraints() {
    let c = BoxConstraints::loose(Size::new(300, 200));
    assert!(!c.is_tight());
    assert_eq!(c.min_width, 0);
    assert_eq!(c.max_width, 300);
    assert_eq!(c.min_height, 0);
    assert_eq!(c.max_height, 200);
    assert_eq!(c.constrain(Size::new(150, 100)), Size::new(150, 100));
    assert_eq!(c.constrain(Size::ZERO), Size::ZERO);
}

fn test_constrain_clamps() {
    let c = BoxConstraints {
        min_width: 50,
        max_width: 200,
        min_height: 30,
        max_height: 100,
    };
    assert_eq!(c.constrain(Size::new(10, 5)), Size::new(50, 30));
    assert_eq!(c.constrain(Size::new(999, 999)), Size::new(200, 100));
    assert_eq!(c.constrain(Size::new(100, 60)), Size::new(100, 60));
}

fn test_deflate() {
    let c = BoxConstraints {
        min_width: 100,
        max_width: 400,
        min_height: 50,
        max_height: 300,
    };
    let insets = EdgeInsets::all(10);
    let d = c.deflate(insets);
    assert_eq!(d.min_width, 80);
    assert_eq!(d.max_width, 380);
    assert_eq!(d.min_height, 30);
    assert_eq!(d.max_height, 280);
}

fn test_unbounded() {
    let c = BoxConstraints::UNBOUNDED;
    assert_eq!(c.min_width, 0);
    assert_eq!(c.max_width, MAX_EXTENT);
    assert_eq!(c.min_height, 0);
    assert_eq!(c.max_height, MAX_EXTENT);
    assert!(!c.is_width_bounded());
    assert!(!c.is_height_bounded());
    assert_eq!(c.constrain(Size::new(9999, 9999)), Size::new(9999, 9999));
}

fn test_rect_contains() {
    let r = Rect::new(10, 20, 100, 50);
    assert!(r.contains(10, 20));
    assert!(r.contains(50, 40));
    assert!(r.contains(109, 69));
    // The upper edges are exclusive.
    assert!(!r.contains(110, 20));
    assert!(!r.contains(10, 70));
    assert!(!r.contains(9, 20));
    assert!(!r.contains(10, 19));
    assert!(!r.contains(200, 200));
}

fn test_rect_intersect() {
    let a = Rect::new(0, 0, 100, 100);
    let b = Rect::new(50, 50, 100, 100);
    let i = a.intersect(&b).expect("should intersect");
    assert_eq!(i, Rect::new(50, 50, 50, 50));
}

fn test_rect_no_intersect() {
    let a = Rect::new(0, 0, 50, 50);
    let b = Rect::new(100, 100, 50, 50);
    assert!(a.intersect(&b).is_none());
    // Touching edges are not an overlap.
    let c = Rect::new(50, 0, 50, 50);
    assert!(a.intersect(&c).is_none());
}

fn make_measure_ctx(style: &StyleSheet) -> MeasureCtx<'_> {
    MeasureCtx { style }
}

fn test_vstack_measure() {
    let style = StyleSheet::dark();
    let mut ctx = make_measure_ctx(&style);
    let children: Vec<Box<dyn Widget>> = vec![
        Box::new(FixedSizeWidget::new(80, 20)),
        Box::new(FixedSizeWidget::new(60, 30)),
        Box::new(FixedSizeWidget::new(100, 10)),
    ];
    let spacing = 5;
    let mut vstack = VStackWidget::new(children, spacing, CrossAxisAlignment::Start);
    let size = vstack.measure(BoxConstraints::UNBOUNDED, &mut ctx);
    assert_eq!(size, Size::new(100, 70));
}

fn test_hstack_measure() {
    let style = StyleSheet::dark();
    let mut ctx = make_measure_ctx(&style);
    let children: Vec<Box<dyn Widget>> = vec![
        Box::new(FixedSizeWidget::new(40, 20)),
        Box::new(FixedSizeWidget::new(60, 30)),
    ];
    let spacing = 10;
    let mut hstack = HStackWidget::new(children, spacing, CrossAxisAlignment::Start);
    let size = hstack.measure(BoxConstraints::UNBOUNDED, &mut ctx);
    assert_eq!(size, Size::new(110, 30));
}

fn test_padding_measure() {
    let style = StyleSheet::dark();
    let mut ctx = make_measure_ctx(&style);
    let child = Box::new(FixedSizeWidget::new(50, 30));
    let insets = EdgeInsets::new(5, 10, 15, 20);
    let mut padding = PaddingWidget::new(insets, child);
    let size = padding.measure(BoxConstraints::UNBOUNDED, &mut ctx);
    assert_eq!(size, Size::new(80, 50));
}

fn test_spacer_measure() {
    let style = StyleSheet::dark();
    let mut ctx = make_measure_ctx(&style);
    let mut spacer = SpacerWidget::new(Length::Px(16));
    let size = spacer.measure(BoxConstraints::UNBOUNDED, &mut ctx);
    assert_eq!(size, Size::new(16, 16));
}

fn test_focus_next() {
    let a = FixedSizeWidget::focusable(10, 10);
    let b = FixedSizeWidget::focusable(10, 10);
    let c = FixedSizeWidget::focusable(10, 10);
    let id_a = a.id();
    let id_b = b.id();

    let children: Vec<Box<dyn Widget>> = vec![Box::new(a), Box::new(b), Box::new(c)];
    let vstack = VStackWidget::new(children, 0, CrossAxisAlignment::Start);
    let mut fm = FocusManager::new();
    fm.rebuild_tab_chain(&vstack);

    fm.move_focus_next();
    assert_eq!(fm.focused(), Some(id_a));

    fm.move_focus_next();
    assert_eq!(fm.focused(), Some(id_b));
}

fn test_focus_prev() {
    let a = FixedSizeWidget::focusable(10, 10);
    let b = FixedSizeWidget::focusable(10, 10);
    let c = FixedSizeWidget::focusable(10, 10);
    let id_c = c.id();

    let children: Vec<Box<dyn Widget>> = vec![Box::new(a), Box::new(b), Box::new(c)];
    let vstack = VStackWidget::new(children, 0, CrossAxisAlignment::Start);
    let mut fm = FocusManager::new();
    fm.rebuild_tab_chain(&vstack);

    // With nothing focused, Shift+Tab lands on the last widget.
    fm.move_focus_prev();
    assert_eq!(fm.focused(), Some(id_c));
}

fn test_focus_wrap() {
    let a = FixedSizeWidget::focusable(10, 10);
    let b = FixedSizeWidget::focusable(10, 10);
    let id_a = a.id();
    let id_b = b.id();

    let children: Vec<Box<dyn Widget>> = vec![Box::new(a), Box::new(b)];
    let vstack = VStackWidget::new(children, 0, CrossAxisAlignment::Start);
    let mut fm = FocusManager::new();
    fm.rebuild_tab_chain(&vstack);

    fm.set_focused(Some(id_b));

    fm.move_focus_next();
    assert_eq!(fm.focused(), Some(id_a));

    fm.move_focus_prev();
    assert_eq!(fm.focused(), Some(id_b));
}

fn test_focus_scope() {
    let a = FixedSizeWidget::focusable(10, 10);
    let b = FixedSizeWidget::focusable(10, 10);
    let c = FixedSizeWidget::focusable(10, 10);
    let id_a = a.id();
    let id_b = b.id();
    let id_c = c.id();

    let children: Vec<Box<dyn Widget>> = vec![Box::new(a), Box::new(b), Box::new(c)];
    let vstack = VStackWidget::new(children, 0, CrossAxisAlignment::Start);
    let mut fm = FocusManager::new();
    fm.rebuild_tab_chain(&vstack);

    fm.set_focused(Some(id_a));
    assert_eq!(fm.focused(), Some(id_a));

    fm.push_scope(vec![id_b, id_c]);
    assert_eq!(fm.focused(), Some(id_b));

    fm.move_focus_next();
    assert_eq!(fm.focused(), Some(id_c));

    // Wraps inside the scope rather than escaping to A.
    fm.move_focus_next();
    assert_eq!(fm.focused(), Some(id_b));

    fm.pop_scope();
    assert_eq!(fm.focused(), Some(id_a));
}

fn test_hit_test_leaf() {
    let mut w = FixedSizeWidget::new(100, 50);
    place_widget(&mut w, Rect::new(10, 20, 100, 50));
    let result = hit_test(&w, 50, 40);
    assert!(result.is_some());
    let ht = result.unwrap();
    assert_eq!(ht.target, w.id());
    assert_eq!(ht.chain.len(), 1);
}

fn test_hit_test_miss() {
    let mut w = FixedSizeWidget::new(100, 50);
    place_widget(&mut w, Rect::new(10, 20, 100, 50));
    let result = hit_test(&w, 0, 0);
    assert!(result.is_none());
    let result2 = hit_test(&w, 200, 200);
    assert!(result2.is_none());
}

fn test_edge_insets_symmetric() {
    let insets = EdgeInsets::symmetric(10, 5);
    assert_eq!(insets.horizontal(), 20);
    assert_eq!(insets.vertical(), 10);
    assert_eq!(insets.left, 10);
    assert_eq!(insets.right, 10);
    assert_eq!(insets.top, 5);
    assert_eq!(insets.bottom, 5);
}

fn test_box_constraints_loosen() {
    let c = BoxConstraints {
        min_width: 50,
        max_width: 200,
        min_height: 30,
        max_height: 100,
    };
    let l = c.loosen();
    assert_eq!(l.min_width, 0);
    assert_eq!(l.max_width, 200);
    assert_eq!(l.min_height, 0);
    assert_eq!(l.max_height, 100);
}

fn test_deflate_unbounded() {
    let c = BoxConstraints::UNBOUNDED;
    let insets = EdgeInsets::all(10);
    let d = c.deflate(insets);
    assert_eq!(d.max_width, MAX_EXTENT);
    assert_eq!(d.max_height, MAX_EXTENT);
    assert_eq!(d.min_width, 0);
    assert_eq!(d.min_height, 0);
}

use super::node::{ContextMenuAt, TableColumn, TableColumnWidth};
use super::widgets::popup::PopupWidget;
use super::widgets::table::TableWidget;

fn table_column(label: &str) -> TableColumn {
    TableColumn {
        label: String::from(label),
        width: TableColumnWidth::Flex(1),
        sort_indicator: None,
    }
}

/// A 3-row, 1-column table laid out at (0,0,200,200) with 20px rows.
fn context_table(selected: Option<usize>) -> TableWidget {
    let rows: Vec<Vec<Box<dyn Widget>>> = (0..3)
        .map(|_| vec![Box::new(FixedSizeWidget::new(50, 20)) as Box<dyn Widget>])
        .collect();
    let mut table = TableWidget::new(
        vec![table_column("Name")],
        rows,
        20,
        selected,
        Some(Box::new(|i: usize| Box::new(i) as Box<dyn std::any::Any>)),
        Some(Box::new(|i: usize| {
            Box::new(format!("header{i}")) as Box<dyn std::any::Any>
        })),
        Some(Box::new(|at: ContextMenuAt| {
            Box::new(at) as Box<dyn std::any::Any>
        })),
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut table,
        BoxConstraints::tight(Size::new(200, 200)),
        &mut ctx,
    );
    place_widget(&mut table, Rect::new(0, 0, 200, 200));
    table
}

fn press(x: i32, y: i32, button: super::event::PointerButton) -> WidgetEvent {
    WidgetEvent::PointerDown { x, y, button }
}

fn test_table_right_click_emits_context_menu() {
    let mut table = context_table(None);
    let mut sink = MessageSink::new();

    // Row 1 spans y=[40,60) once the 20px header is skipped.
    let resp = table.event(
        &press(10, 45, super::event::PointerButton::Right),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());

    let requests = sink.drain_typed::<ContextMenuAt>();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].row, 1);
    assert_eq!((requests[0].x, requests[0].y), (10, 45));
}

/// The menu acts on the selection, so opening it must move the selection too.
fn test_table_right_click_selects_row() {
    let mut table = context_table(Some(0));
    let mut sink = MessageSink::new();
    // Row 2 is the last of three: y=[60,80) once the 20px header is skipped.
    table.event(
        &press(10, 70, super::event::PointerButton::Right),
        EventPhase::Target,
        &mut sink,
    );
    let selections = sink.drain_typed::<usize>();
    assert_eq!(selections, vec![2]);
}

fn test_table_left_click_emits_no_context_menu() {
    let mut table = context_table(None);
    let mut sink = MessageSink::new();
    table.event(
        &press(10, 45, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(sink.drain_typed::<ContextMenuAt>().is_empty());
    assert_eq!(sink.drain_typed::<usize>(), vec![1]);
}

fn test_table_right_click_header_is_inert() {
    let mut table = context_table(None);
    let mut sink = MessageSink::new();
    let resp = table.event(
        &press(10, 5, super::event::PointerButton::Right),
        EventPhase::Target,
        &mut sink,
    );
    assert!(!resp.is_consumed());
    assert!(sink.drain_typed::<ContextMenuAt>().is_empty());
    assert!(sink.drain_typed::<String>().is_empty());
}

/// Keyboard parity: the Menu key raises the same request, anchored to the row.
fn test_table_menu_key_emits_context_menu() {
    let mut table = context_table(Some(1));
    let mut sink = MessageSink::new();
    let resp = table.event(
        &WidgetEvent::KeyDown {
            key: super::event::Key::Named(super::event::NamedKey::Menu),
            modifiers: super::event::Modifiers::default(),
            repeat: false,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    let requests = sink.drain_typed::<ContextMenuAt>();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].row, 1);
    // Anchored at the row's bottom-left: header 20 + row 1 ends at y=60.
    assert_eq!((requests[0].x, requests[0].y), (0, 60));
}

fn test_table_menu_key_without_selection_is_inert() {
    let mut table = context_table(None);
    let mut sink = MessageSink::new();
    let resp = table.event(
        &WidgetEvent::KeyDown {
            key: super::event::Key::Named(super::event::NamedKey::Menu),
            modifiers: super::event::Modifiers::default(),
            repeat: false,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert!(!resp.is_consumed());
    assert!(sink.drain_typed::<ContextMenuAt>().is_empty());
}

fn popup_at(x: i32, y: i32, w: i32, h: i32) -> PopupWidget {
    let mut popup = PopupWidget::new(
        x,
        y,
        Box::new(FixedSizeWidget::new(w, h)),
        Some(Box::new(|| {
            Box::new(String::from("dismiss")) as Box<dyn std::any::Any>
        })),
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(
        &mut popup,
        BoxConstraints::tight(Size::new(200, 200)),
        &mut ctx,
    );
    place_widget(&mut popup, Rect::new(0, 0, 200, 200));
    popup
}

fn test_popup_places_child_at_anchor() {
    let popup = popup_at(30, 40, 60, 50);
    let child = popup.children()[0].layout_rect();
    assert_eq!((child.x, child.y), (30, 40));
}

/// Near the right or bottom edge the child flips back over the anchor rather
/// than being clipped, so it never covers the pointer that opened it.
fn test_popup_flips_at_edges() {
    let popup = popup_at(190, 195, 60, 50);
    let child = popup.children()[0].layout_rect();
    assert_eq!((child.x, child.y), (130, 145));
}

/// A child too large to flip is clamped inside the parent instead.
fn test_popup_clamps_oversized_child() {
    let popup = popup_at(190, 190, 300, 300);
    let child = popup.children()[0].layout_rect();
    assert_eq!((child.x, child.y), (0, 0));
}

fn test_popup_click_outside_dismisses() {
    let mut popup = popup_at(30, 40, 60, 50);
    let mut sink = MessageSink::new();
    let resp = popup.event(
        &press(5, 5, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    assert_eq!(sink.drain_typed::<String>().len(), 1);
}

fn test_popup_click_inside_does_not_dismiss() {
    let mut popup = popup_at(30, 40, 60, 50);
    let mut sink = MessageSink::new();
    popup.event(
        &press(35, 45, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(sink.drain_typed::<String>().is_empty());
}

fn test_popup_escape_dismisses() {
    let mut popup = popup_at(30, 40, 60, 50);
    let mut sink = MessageSink::new();
    let resp = popup.event(
        &WidgetEvent::KeyDown {
            key: super::event::Key::Named(super::event::NamedKey::Escape),
            modifiers: super::event::Modifiers::default(),
            repeat: false,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    assert_eq!(sink.drain_typed::<String>().len(), 1);
}

/// A popup is modal: events its child ignored must not reach the tree below.
fn test_popup_swallows_unhandled_events() {
    let mut popup = popup_at(30, 40, 60, 50);
    let mut sink = MessageSink::new();
    let resp = popup.event(
        &WidgetEvent::Scroll {
            delta_x: 0,
            delta_y: 10,
        },
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
}

use super::widgets::dialog::DialogWidget;

/// A dialog laid out in `window`, with two 80x30 actions and a 200x40 body.
fn dialog_in(window: Size) -> DialogWidget {
    let mut dialog = DialogWidget::new(
        String::from("Kill task?"),
        Box::new(FixedSizeWidget::new(200, 40)),
        vec![
            Box::new(ProbeButton::new("kill", 80, 30)) as Box<dyn Widget>,
            Box::new(ProbeButton::new("cancel", 80, 30)) as Box<dyn Widget>,
        ],
        Some(Box::new(|| {
            Box::new(String::from("dismiss")) as Box<dyn std::any::Any>
        })),
    );
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    measure_widget(&mut dialog, BoxConstraints::tight(window), &mut ctx);
    place_widget(&mut dialog, Rect::new(0, 0, window.width, window.height));
    dialog
}

fn test_dialog_places_children_inside_card() {
    let window = Size::new(640, 444);
    let dialog = dialog_in(window);
    let card = dialog.card_rect();

    assert!(
        card.width > 0 && card.height > 0,
        "card degenerate: {card:?}"
    );
    assert!(
        card.x >= 0 && card.y >= 0,
        "card starts off-screen: {card:?}"
    );
    assert!(
        card.x + card.width <= window.width && card.y + card.height <= window.height,
        "card overflows the window: {card:?}"
    );

    for action in dialog.children() {
        let r = action.layout_rect();
        assert!(
            r.x >= card.x
                && r.y >= card.y
                && r.x + r.width <= card.x + card.width
                && r.y + r.height <= card.y + card.height,
            "action {r:?} outside card {card:?}"
        );
    }
}

fn test_dialog_card_height_covers_content() {
    let dialog = dialog_in(Size::new(640, 444));
    let card = dialog.card_rect();
    // title row + 40px content + 30px actions + padding.
    assert!(card.height >= 40 + 30, "card too short: {card:?}");
}

fn test_dialog_card_is_centered() {
    let window = Size::new(640, 444);
    let dialog = dialog_in(window);
    let card = dialog.card_rect();
    assert_eq!(card.x, (window.width - card.width) / 2);
    assert_eq!(card.y, (window.height - card.height) / 2);
}

fn test_dialog_click_routes_to_action_under_pointer() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let second = dialog.children()[1].layout_rect();
    let (cx, cy) = (second.x + second.width / 2, second.y + second.height / 2);

    let mut sink = MessageSink::new();
    dialog.event(
        &press(cx, cy, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert_eq!(sink.drain_typed::<String>(), vec![String::from("cancel")]);
}

fn test_dialog_backdrop_click_dismisses() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let mut sink = MessageSink::new();
    let resp = dialog.event(
        &press(2, 2, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    assert_eq!(sink.drain_typed::<String>(), vec![String::from("dismiss")]);
}

/// A press inside the card that hits no action is swallowed, not passed down.
fn test_dialog_is_modal_over_its_parent() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let card = dialog.card_rect();
    let mut sink = MessageSink::new();
    let resp = dialog.event(
        &press(card.x + 2, card.y + 2, super::event::PointerButton::Left),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    assert!(sink.drain_typed::<String>().is_empty());
}

fn key(named: super::event::NamedKey, shift: bool) -> WidgetEvent {
    WidgetEvent::KeyDown {
        key: super::event::Key::Named(named),
        modifiers: super::event::Modifiers {
            shift,
            ..Default::default()
        },
        repeat: false,
    }
}

fn test_dialog_enter_without_selection_fires_nothing() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let mut sink = MessageSink::new();
    dialog.event(
        &key(super::event::NamedKey::Enter, false),
        EventPhase::Target,
        &mut sink,
    );
    assert!(sink.drain_typed::<String>().is_empty());
}

fn test_dialog_keyboard_selects_then_activates() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let mut sink = MessageSink::new();
    dialog.event(
        &key(super::event::NamedKey::Tab, false),
        EventPhase::Target,
        &mut sink,
    );
    dialog.event(
        &key(super::event::NamedKey::Tab, false),
        EventPhase::Target,
        &mut sink,
    );
    dialog.event(
        &key(super::event::NamedKey::Enter, false),
        EventPhase::Target,
        &mut sink,
    );
    assert_eq!(sink.drain_typed::<String>(), vec![String::from("cancel")]);
}

/// Escape dismisses rather than activating whatever is selected.
fn test_dialog_escape_dismisses() {
    let mut dialog = dialog_in(Size::new(640, 444));
    let mut sink = MessageSink::new();
    let resp = dialog.event(
        &key(super::event::NamedKey::Escape, false),
        EventPhase::Target,
        &mut sink,
    );
    assert!(resp.is_consumed());
    assert_eq!(sink.drain_typed::<String>(), vec![String::from("dismiss")]);
}

fn test_unbounded_extent_survives_padding_arithmetic() {
    let c = BoxConstraints::UNBOUNDED;
    assert!(!c.is_width_bounded());
    assert!(!c.is_height_bounded());
    // The sum a card computes: title + content + two paddings.
    let total = c.max_height + 16 + 16 + 32;
    assert!(total > 0, "unbounded extent wrapped to {total}");
}

/// Deflating an unbounded axis must leave it unbounded, or a scroll view's
/// child suddenly believes it has a finite budget.
fn test_deflate_preserves_unboundedness() {
    let d = BoxConstraints::UNBOUNDED.deflate(EdgeInsets::all(10));
    assert!(!d.is_width_bounded());
    assert!(!d.is_height_bounded());
    assert_eq!(d.max_width, MAX_EXTENT);
}

fn test_measure_widget_records_size() {
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    let mut w = FixedSizeWidget::new(70, 25);
    assert_eq!(w.measured_size(), Size::ZERO);
    let size = measure_widget(&mut w, BoxConstraints::UNBOUNDED, &mut ctx);
    assert_eq!(size, Size::new(70, 25));
    assert_eq!(w.measured_size(), Size::new(70, 25));
}

/// `place_widget` is what records the rect, so `layout_rect` reflects the
/// placement even for a leaf that implements no `layout` at all.
fn test_place_widget_records_rect() {
    let mut w = FixedSizeWidget::new(70, 25);
    place_widget(&mut w, Rect::new(5, 6, 70, 25));
    assert_eq!(w.layout_rect(), Rect::new(5, 6, 70, 25));
}

/// A ZStack's layers all get the full area: an overlay covers its siblings
/// rather than displacing them.
fn test_zstack_layers_share_the_full_rect() {
    let style = StyleSheet::dark();
    let mut ctx = MeasureCtx { style: &style };
    let children: Vec<Box<dyn Widget>> = vec![
        Box::new(FixedSizeWidget::new(50, 20)),
        Box::new(FixedSizeWidget::new(10, 10)),
    ];
    let mut z = super::layout::ZStackWidget::new(children);
    measure_widget(&mut z, BoxConstraints::tight(Size::new(200, 100)), &mut ctx);
    place_widget(&mut z, Rect::new(0, 0, 200, 100));
    for child in z.children() {
        assert_eq!(child.layout_rect(), Rect::new(0, 0, 200, 100));
    }
}

/// Every appkit unit test, for a host `cargo test` run and for the
/// `/bin/appkit_test` userland binary that reports them over KTAP.
pub fn cases() -> &'static [(&'static str, fn())] {
    &[
        ("tight_constraints", test_tight_constraints),
        ("loose_constraints", test_loose_constraints),
        ("constrain_clamps", test_constrain_clamps),
        ("deflate", test_deflate),
        ("unbounded", test_unbounded),
        ("rect_contains", test_rect_contains),
        ("rect_intersect", test_rect_intersect),
        ("rect_no_intersect", test_rect_no_intersect),
        ("vstack_measure", test_vstack_measure),
        ("hstack_measure", test_hstack_measure),
        ("padding_measure", test_padding_measure),
        ("spacer_measure", test_spacer_measure),
        ("focus_next", test_focus_next),
        ("focus_prev", test_focus_prev),
        ("focus_wrap", test_focus_wrap),
        ("focus_scope", test_focus_scope),
        ("hit_test_leaf", test_hit_test_leaf),
        ("hit_test_miss", test_hit_test_miss),
        ("edge_insets_symmetric", test_edge_insets_symmetric),
        ("box_constraints_loosen", test_box_constraints_loosen),
        ("deflate_unbounded", test_deflate_unbounded),
        (
            "table_right_click_emits_context_menu",
            test_table_right_click_emits_context_menu,
        ),
        (
            "table_right_click_selects_row",
            test_table_right_click_selects_row,
        ),
        (
            "table_left_click_emits_no_context_menu",
            test_table_left_click_emits_no_context_menu,
        ),
        (
            "table_right_click_header_is_inert",
            test_table_right_click_header_is_inert,
        ),
        (
            "table_menu_key_emits_context_menu",
            test_table_menu_key_emits_context_menu,
        ),
        (
            "table_menu_key_without_selection_is_inert",
            test_table_menu_key_without_selection_is_inert,
        ),
        (
            "popup_places_child_at_anchor",
            test_popup_places_child_at_anchor,
        ),
        ("popup_flips_at_edges", test_popup_flips_at_edges),
        (
            "popup_clamps_oversized_child",
            test_popup_clamps_oversized_child,
        ),
        (
            "popup_click_outside_dismisses",
            test_popup_click_outside_dismisses,
        ),
        (
            "popup_click_inside_does_not_dismiss",
            test_popup_click_inside_does_not_dismiss,
        ),
        ("popup_escape_dismisses", test_popup_escape_dismisses),
        (
            "popup_swallows_unhandled_events",
            test_popup_swallows_unhandled_events,
        ),
        (
            "dialog_places_children_inside_card",
            test_dialog_places_children_inside_card,
        ),
        (
            "dialog_card_height_covers_content",
            test_dialog_card_height_covers_content,
        ),
        ("dialog_card_is_centered", test_dialog_card_is_centered),
        (
            "dialog_click_routes_to_action_under_pointer",
            test_dialog_click_routes_to_action_under_pointer,
        ),
        (
            "dialog_backdrop_click_dismisses",
            test_dialog_backdrop_click_dismisses,
        ),
        (
            "dialog_is_modal_over_its_parent",
            test_dialog_is_modal_over_its_parent,
        ),
        (
            "unbounded_extent_survives_padding_arithmetic",
            test_unbounded_extent_survives_padding_arithmetic,
        ),
        (
            "deflate_preserves_unboundedness",
            test_deflate_preserves_unboundedness,
        ),
        (
            "measure_widget_records_size",
            test_measure_widget_records_size,
        ),
        ("place_widget_records_rect", test_place_widget_records_rect),
        (
            "zstack_layers_share_the_full_rect",
            test_zstack_layers_share_the_full_rect,
        ),
        (
            "dialog_enter_without_selection_fires_nothing",
            test_dialog_enter_without_selection_fires_nothing,
        ),
        (
            "dialog_keyboard_selects_then_activates",
            test_dialog_keyboard_selects_then_activates,
        ),
        ("dialog_escape_dismisses", test_dialog_escape_dismisses),
    ]
}

pub fn run_all_tests() -> bool {
    let tests = cases();

    let mut passed = 0usize;
    let mut failed = 0usize;

    for (name, func) in tests {
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| func())).is_ok();
        if ok {
            passed += 1;
        } else {
            eprintln!("[FAIL] ui::tests::{}", name);
            failed += 1;
        }
    }

    eprintln!(
        "[ui::tests] {} passed, {} failed, {} total",
        passed,
        failed,
        passed + failed
    );
    failed == 0
}

#[cfg(test)]
mod cfg_tests {
    use super::*;

    #[test]
    fn tight_constraints() {
        test_tight_constraints();
    }
    #[test]
    fn loose_constraints() {
        test_loose_constraints();
    }
    #[test]
    fn constrain_clamps() {
        test_constrain_clamps();
    }
    #[test]
    fn deflate() {
        test_deflate();
    }
    #[test]
    fn unbounded() {
        test_unbounded();
    }
    #[test]
    fn rect_contains() {
        test_rect_contains();
    }
    #[test]
    fn rect_intersect() {
        test_rect_intersect();
    }
    #[test]
    fn rect_no_intersect() {
        test_rect_no_intersect();
    }
    #[test]
    fn vstack_measure() {
        test_vstack_measure();
    }
    #[test]
    fn hstack_measure() {
        test_hstack_measure();
    }
    #[test]
    fn padding_measure() {
        test_padding_measure();
    }
    #[test]
    fn spacer_measure() {
        test_spacer_measure();
    }
    #[test]
    fn focus_next() {
        test_focus_next();
    }
    #[test]
    fn focus_prev() {
        test_focus_prev();
    }
    #[test]
    fn focus_wrap() {
        test_focus_wrap();
    }
    #[test]
    fn focus_scope() {
        test_focus_scope();
    }
    #[test]
    fn hit_test_leaf() {
        test_hit_test_leaf();
    }
    #[test]
    fn hit_test_miss() {
        test_hit_test_miss();
    }
    #[test]
    fn edge_insets_symmetric() {
        test_edge_insets_symmetric();
    }
    #[test]
    fn box_constraints_loosen() {
        test_box_constraints_loosen();
    }
    #[test]
    fn deflate_unbounded() {
        test_deflate_unbounded();
    }
    #[test]
    fn table_right_click_emits_context_menu() {
        test_table_right_click_emits_context_menu();
    }
    #[test]
    fn table_right_click_selects_row() {
        test_table_right_click_selects_row();
    }
    #[test]
    fn table_left_click_emits_no_context_menu() {
        test_table_left_click_emits_no_context_menu();
    }
    #[test]
    fn table_right_click_header_is_inert() {
        test_table_right_click_header_is_inert();
    }
    #[test]
    fn table_menu_key_emits_context_menu() {
        test_table_menu_key_emits_context_menu();
    }
    #[test]
    fn table_menu_key_without_selection_is_inert() {
        test_table_menu_key_without_selection_is_inert();
    }
    #[test]
    fn popup_places_child_at_anchor() {
        test_popup_places_child_at_anchor();
    }
    #[test]
    fn popup_flips_at_edges() {
        test_popup_flips_at_edges();
    }
    #[test]
    fn popup_clamps_oversized_child() {
        test_popup_clamps_oversized_child();
    }
    #[test]
    fn popup_click_outside_dismisses() {
        test_popup_click_outside_dismisses();
    }
    #[test]
    fn popup_click_inside_does_not_dismiss() {
        test_popup_click_inside_does_not_dismiss();
    }
    #[test]
    fn popup_escape_dismisses() {
        test_popup_escape_dismisses();
    }
    #[test]
    fn popup_swallows_unhandled_events() {
        test_popup_swallows_unhandled_events();
    }
    #[test]
    fn dialog_places_children_inside_card() {
        test_dialog_places_children_inside_card();
    }
    #[test]
    fn dialog_card_height_covers_content() {
        test_dialog_card_height_covers_content();
    }
    #[test]
    fn dialog_card_is_centered() {
        test_dialog_card_is_centered();
    }
    #[test]
    fn dialog_click_routes_to_action_under_pointer() {
        test_dialog_click_routes_to_action_under_pointer();
    }
    #[test]
    fn dialog_backdrop_click_dismisses() {
        test_dialog_backdrop_click_dismisses();
    }
    #[test]
    fn dialog_is_modal_over_its_parent() {
        test_dialog_is_modal_over_its_parent();
    }
    #[test]
    fn unbounded_extent_survives_padding_arithmetic() {
        test_unbounded_extent_survives_padding_arithmetic();
    }
    #[test]
    fn deflate_preserves_unboundedness() {
        test_deflate_preserves_unboundedness();
    }
    #[test]
    fn measure_widget_records_size() {
        test_measure_widget_records_size();
    }
    #[test]
    fn place_widget_records_rect() {
        test_place_widget_records_rect();
    }
    #[test]
    fn zstack_layers_share_the_full_rect() {
        test_zstack_layers_share_the_full_rect();
    }
    #[test]
    fn dialog_enter_without_selection_fires_nothing() {
        test_dialog_enter_without_selection_fires_nothing();
    }
    #[test]
    fn dialog_keyboard_selects_then_activates() {
        test_dialog_keyboard_selects_then_activates();
    }
    #[test]
    fn dialog_escape_dismisses() {
        test_dialog_escape_dismisses();
    }
}
