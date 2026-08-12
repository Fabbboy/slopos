use std::any::Any;

use super::constraints::{BoxConstraints, Rect, Size};
use super::node::{ContextMenuAt, Node};
use super::paint::PaintContext;
use super::style::StyleSheet;
use super::traits::{MeasureCtx, Widget, measure_widget, place_widget};

/// Build a retained widget tree from a declarative `Node<M>` tree.
///
/// `M` is the app's message type. The function bridges from the generic `Node<M>`
/// to type-erased widget storage (`Box<dyn Any>`, `Box<dyn Fn(...) -> Box<dyn Any>>`).
///
/// For v1, this is a simple rebuild-from-scratch approach.
/// Future versions will diff old vs new and patch incrementally.
pub fn build_widget_tree<M: Clone + 'static>(node: &Node<M>) -> Box<dyn Widget> {
    use super::widgets;

    match node {
        Node::Label {
            text,
            alignment,
            wrap,
            max_lines,
        } => Box::new(widgets::label::LabelWidget::new(
            text.clone(),
            *alignment,
            *wrap,
            *max_lines,
        )),
        Node::Button {
            label,
            on_press,
            style,
            enabled,
        } => {
            let erased: Option<Box<dyn Fn() -> Box<dyn Any>>> = on_press.as_ref().map(|m| {
                let m = m.clone();
                Box::new(move || Box::new(m.clone()) as Box<dyn Any>)
                    as Box<dyn Fn() -> Box<dyn Any>>
            });
            Box::new(widgets::button::ButtonWidget::new(
                label.clone(),
                erased,
                *style,
                *enabled,
            ))
        }
        Node::TextField {
            text,
            placeholder,
            on_change,
            max_length,
            read_only,
        } => {
            let erased: Option<Box<dyn Fn(String) -> Box<dyn Any>>> = on_change.map(|f| {
                Box::new(move |s: String| Box::new(f(s)) as Box<dyn Any>)
                    as Box<dyn Fn(String) -> Box<dyn Any>>
            });
            Box::new(widgets::text_field::TextFieldWidget::new(
                text.clone(),
                placeholder.clone(),
                erased,
                *max_length,
                *read_only,
            ))
        }
        Node::Checkbox {
            checked,
            label,
            on_toggle,
            enabled,
        } => {
            let erased: Option<Box<dyn Fn() -> Box<dyn Any>>> = on_toggle.as_ref().map(|m| {
                let m = m.clone();
                Box::new(move || Box::new(m.clone()) as Box<dyn Any>)
                    as Box<dyn Fn() -> Box<dyn Any>>
            });
            Box::new(widgets::checkbox::CheckboxWidget::new(
                *checked,
                label.clone(),
                erased,
                *enabled,
            ))
        }
        Node::Divider => Box::new(widgets::separator::SeparatorWidget::new()),
        Node::Image {
            image,
            scale,
            sampling,
        } => Box::new(widgets::image::ImageWidget::new(
            image.clone(),
            *scale,
            *sampling,
        )),
        Node::ScrollView {
            child,
            direction,
            show_scrollbar,
            scroll_y,
            on_scroll,
        } => {
            let child_widget = build_widget_tree(child);
            let erased: Option<Box<dyn Fn(i32) -> Box<dyn Any>>> = on_scroll.map(|f| {
                Box::new(move |v: i32| Box::new(f(v)) as Box<dyn Any>)
                    as Box<dyn Fn(i32) -> Box<dyn Any>>
            });
            Box::new(widgets::scroll_view::ScrollViewWidget::with_scroll(
                child_widget,
                *direction,
                *show_scrollbar,
                *scroll_y,
                erased,
            ))
        }
        Node::ListView {
            item_height,
            selected,
            on_select,
            items,
        } => {
            let item_widgets: Vec<Box<dyn Widget>> =
                items.iter().map(|n| build_widget_tree(n)).collect();
            let erased: Option<Box<dyn Fn(usize) -> Box<dyn Any>>> = on_select.map(|f| {
                Box::new(move |i: usize| Box::new(f(i)) as Box<dyn Any>)
                    as Box<dyn Fn(usize) -> Box<dyn Any>>
            });
            Box::new(widgets::list_view::ListViewWidget::new(
                *item_height,
                *selected,
                erased,
                item_widgets,
            ))
        }
        Node::TabBar {
            tabs,
            active,
            on_change,
            content,
        } => {
            let content_widgets: Vec<Box<dyn Widget>> =
                content.iter().map(|n| build_widget_tree(n)).collect();
            let erased: Option<Box<dyn Fn(usize) -> Box<dyn Any>>> = on_change.map(|f| {
                Box::new(move |i: usize| Box::new(f(i)) as Box<dyn Any>)
                    as Box<dyn Fn(usize) -> Box<dyn Any>>
            });
            Box::new(widgets::tab_bar::TabBarWidget::new(
                tabs.clone(),
                *active,
                erased,
                content_widgets,
            ))
        }
        Node::Menu { items, on_action } => {
            let erased: Option<Box<dyn Fn(usize) -> Box<dyn Any>>> = on_action.map(|f| {
                Box::new(move |i: usize| Box::new(f(i)) as Box<dyn Any>)
                    as Box<dyn Fn(usize) -> Box<dyn Any>>
            });
            Box::new(widgets::menu::MenuWidget::new(items.clone(), erased))
        }
        Node::Table {
            columns,
            rows,
            row_height,
            selected,
            on_select,
            on_header_click,
            on_context_menu,
        } => {
            let row_widgets: Vec<Vec<Box<dyn Widget>>> = rows
                .iter()
                .map(|row| row.iter().map(|n| build_widget_tree(n)).collect())
                .collect();
            let erased_select: Option<Box<dyn Fn(usize) -> Box<dyn Any>>> = on_select.map(|f| {
                Box::new(move |i: usize| Box::new(f(i)) as Box<dyn Any>)
                    as Box<dyn Fn(usize) -> Box<dyn Any>>
            });
            let erased_header: Option<Box<dyn Fn(usize) -> Box<dyn Any>>> =
                on_header_click.map(|f| {
                    Box::new(move |i: usize| Box::new(f(i)) as Box<dyn Any>)
                        as Box<dyn Fn(usize) -> Box<dyn Any>>
                });
            let erased_context: Option<Box<dyn Fn(ContextMenuAt) -> Box<dyn Any>>> =
                on_context_menu.map(|f| {
                    Box::new(move |at: ContextMenuAt| Box::new(f(at)) as Box<dyn Any>)
                        as Box<dyn Fn(ContextMenuAt) -> Box<dyn Any>>
                });
            Box::new(widgets::table::TableWidget::new(
                columns.clone(),
                row_widgets,
                *row_height,
                *selected,
                erased_select,
                erased_header,
                erased_context,
            ))
        }
        Node::Dialog {
            title,
            content,
            actions,
            on_dismiss,
        } => {
            let content_widget = build_widget_tree(content);
            let action_widgets: Vec<Box<dyn Widget>> =
                actions.iter().map(|n| build_widget_tree(n)).collect();
            let erased: Option<Box<dyn Fn() -> Box<dyn Any>>> = on_dismiss.as_ref().map(|m| {
                let m = m.clone();
                Box::new(move || Box::new(m.clone()) as Box<dyn Any>)
                    as Box<dyn Fn() -> Box<dyn Any>>
            });
            Box::new(widgets::dialog::DialogWidget::new(
                title.clone(),
                content_widget,
                action_widgets,
                erased,
            ))
        }
        Node::Popup {
            x,
            y,
            child,
            on_dismiss,
        } => {
            let child_widget = build_widget_tree(child);
            let erased: Option<Box<dyn Fn() -> Box<dyn Any>>> = on_dismiss.as_ref().map(|m| {
                let m = m.clone();
                Box::new(move || Box::new(m.clone()) as Box<dyn Any>)
                    as Box<dyn Fn() -> Box<dyn Any>>
            });
            Box::new(widgets::popup::PopupWidget::new(
                *x,
                *y,
                child_widget,
                erased,
            ))
        }
        Node::ProgressBar {
            value,
            label,
            color,
        } => Box::new(widgets::progress_bar::ProgressBarWidget::new(
            *value,
            label.clone(),
            *color,
        )),
        Node::StyledLabel {
            text,
            color,
            alignment,
        } => Box::new(widgets::styled_label::StyledLabelWidget::new(
            text.clone(),
            *color,
            *alignment,
        )),
        Node::VStack {
            children,
            spacing,
            align,
        } => {
            let child_widgets: Vec<Box<dyn Widget>> =
                children.iter().map(|n| build_widget_tree(n)).collect();
            Box::new(super::layout::VStackWidget::new(
                child_widgets,
                *spacing,
                *align,
            ))
        }
        Node::HStack {
            children,
            spacing,
            align,
        } => {
            let child_widgets: Vec<Box<dyn Widget>> =
                children.iter().map(|n| build_widget_tree(n)).collect();
            Box::new(super::layout::HStackWidget::new(
                child_widgets,
                *spacing,
                *align,
            ))
        }
        Node::ZStack { children } => {
            let child_widgets: Vec<Box<dyn Widget>> =
                children.iter().map(|n| build_widget_tree(n)).collect();
            Box::new(super::layout::ZStackWidget::new(child_widgets))
        }
        Node::Padding { padding, child } => {
            let child_widget = build_widget_tree(child);
            Box::new(super::layout::PaddingWidget::new(*padding, child_widget))
        }
        Node::Spacer { size } => Box::new(super::layout::SpacerWidget::new(*size)),
        Node::Expand { weight, child } => {
            let child_widget = build_widget_tree(child);
            Box::new(super::layout::ExpandWidget::new(*weight, child_widget))
        }
        Node::Background { color, child } => {
            let child_widget = build_widget_tree(child);
            Box::new(super::layout::BackgroundWidget::new(*color, child_widget))
        }
        Node::SizedBox {
            width,
            height,
            child,
        } => {
            let child_widget = build_widget_tree(child);
            Box::new(super::layout::SizedBoxWidget::new(
                *width,
                *height,
                child_widget,
            ))
        }
        Node::Canvas {
            width: _,
            height: _,
        } => Box::new(widgets::label::LabelWidget::new(
            String::new(),
            super::constraints::TextAlignment::Start,
            false,
            None,
        )),
        Node::Empty => Box::new(widgets::label::LabelWidget::new(
            String::new(),
            super::constraints::TextAlignment::Start,
            false,
            None,
        )),
    }
}

/// Perform a full measure + layout pass on the widget tree.
pub fn layout_tree(root: &mut dyn Widget, window_size: Size, style: &StyleSheet) {
    let constraints = BoxConstraints::tight(window_size);
    let mut ctx = MeasureCtx { style };
    measure_widget(root, constraints, &mut ctx);
    place_widget(root, Rect::new(0, 0, window_size.width, window_size.height));
}

/// Paint the entire widget tree.
pub fn paint_tree(root: &dyn Widget, ctx: &mut PaintContext) {
    root.paint(ctx);
}
