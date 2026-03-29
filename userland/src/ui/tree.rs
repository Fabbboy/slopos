use super::constraints::{BoxConstraints, Rect, Size};
use super::node::Node;
use super::paint::PaintContext;
use super::style::StyleSheet;
use super::traits::{MeasureCtx, Widget};

/// Build a retained widget tree from a declarative Node tree.
///
/// For v1, this is a simple rebuild-from-scratch approach.
/// Future versions will diff old vs new and patch incrementally.
pub fn build_widget_tree(node: &Node) -> Box<dyn Widget> {
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
        } => Box::new(widgets::button::ButtonWidget::new(
            label.clone(),
            *on_press,
            *style,
            *enabled,
        )),
        Node::TextField {
            text,
            placeholder,
            on_change,
            max_length,
            read_only,
        } => Box::new(widgets::text_field::TextFieldWidget::new(
            text.clone(),
            placeholder.clone(),
            *on_change,
            *max_length,
            *read_only,
        )),
        Node::Checkbox {
            checked,
            label,
            on_toggle,
            enabled,
        } => Box::new(widgets::checkbox::CheckboxWidget::new(
            *checked,
            label.clone(),
            *on_toggle,
            *enabled,
        )),
        Node::Divider => Box::new(widgets::separator::SeparatorWidget::new()),
        Node::Image {
            width,
            height,
            scale,
        } => Box::new(widgets::image::ImageWidget::new(*width, *height, *scale)),
        Node::ScrollView {
            child,
            direction,
            show_scrollbar,
            scroll_y,
            on_scroll,
        } => {
            let child_widget = build_widget_tree(child);
            Box::new(widgets::scroll_view::ScrollViewWidget::with_scroll(
                child_widget,
                *direction,
                *show_scrollbar,
                *scroll_y,
                *on_scroll,
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
            Box::new(widgets::list_view::ListViewWidget::new(
                *item_height,
                *selected,
                *on_select,
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
            Box::new(widgets::tab_bar::TabBarWidget::new(
                tabs.clone(),
                *active,
                *on_change,
                content_widgets,
            ))
        }
        Node::Menu { items, on_action } => {
            Box::new(widgets::menu::MenuWidget::new(items.clone(), *on_action))
        }
        Node::Table {
            columns,
            rows,
            row_height,
            selected,
            on_select,
            on_header_click,
        } => {
            let row_widgets: Vec<Vec<Box<dyn Widget>>> = rows
                .iter()
                .map(|row| row.iter().map(|n| build_widget_tree(n)).collect())
                .collect();
            Box::new(widgets::table::TableWidget::new(
                columns.clone(),
                row_widgets,
                *row_height,
                *selected,
                *on_select,
                *on_header_click,
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
            Box::new(widgets::dialog::DialogWidget::new(
                title.clone(),
                content_widget,
                action_widgets,
                *on_dismiss,
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
    let _size = root.measure(constraints, &mut ctx);
    root.layout(Rect::new(0, 0, window_size.width, window_size.height));
}

/// Paint the entire widget tree.
pub fn paint_tree(root: &dyn Widget, ctx: &mut PaintContext) {
    root.paint(ctx);
}
