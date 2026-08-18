use slopos_abi::draw::Color32;
use slopos_gfx::image::ImageSampling;
use std::sync::Arc;

use super::constraints::{
    CrossAxisAlignment, EdgeInsets, ImageScale, Length, ScrollDirection, ScrollbarVisibility,
    TextAlignment,
};
use super::event::{Key, Modifiers};

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ButtonStyle {
    #[default]
    Primary,
    Secondary,
    Destructive,
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: &'static str,
    pub shortcut: Option<&'static str>,
    pub enabled: bool,
    pub kind: MenuItemKind,
}

#[derive(Clone, Debug)]
pub enum MenuItemKind {
    Action,
    Separator,
    Submenu(Vec<MenuItem>),
}

#[derive(Copy, Clone, Debug)]
pub enum TableColumnWidth {
    /// Fixed pixel width.
    Fixed(i32),
    /// Proportional flex weight.
    Flex(u16),
}

#[derive(Copy, Clone, Debug)]
pub enum SortIndicator {
    Ascending,
    Descending,
}

/// `x`/`y` are window coordinates; for a keyboard-raised request (Menu key,
/// Shift+F10) they are the selected row's left edge.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ContextMenuAt {
    pub row: usize,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug)]
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<[Color32]>,
}

impl ImageData {
    pub fn new(width: u32, height: u32, pixels: Vec<Color32>) -> Option<Self> {
        let required = (width as usize).checked_mul(height as usize)?;
        if width == 0 || height == 0 || pixels.len() < required {
            return None;
        }
        Some(Self {
            width,
            height,
            pixels: Arc::from(pixels.into_boxed_slice()),
        })
    }
}

#[derive(Clone, Debug)]
pub struct TableColumn {
    pub label: String,
    pub width: TableColumnWidth,
    pub sort_indicator: Option<SortIndicator>,
}

/// Declarative tree an app builds in `view()`; the framework diffs it against
/// the previous one to update retained widgets. Widgets emit the application's
/// own message type `M` rather than opaque integer IDs.
pub enum Node<M> {
    Label {
        text: String,
        alignment: TextAlignment,
        wrap: bool,
        max_lines: Option<u32>,
    },
    Button {
        label: String,
        /// `None` leaves the button non-interactive; `Some` emits on click.
        on_press: Option<M>,
        style: ButtonStyle,
        enabled: bool,
    },
    TextField {
        text: String,
        placeholder: String,
        on_change: Option<fn(String) -> M>,
        max_length: Option<usize>,
        read_only: bool,
    },
    Checkbox {
        checked: bool,
        label: String,
        on_toggle: Option<M>,
        enabled: bool,
    },
    /// Orientation follows the parent: horizontal in a VStack, vertical in an HStack.
    Divider,
    Image {
        image: ImageData,
        scale: ImageScale,
        sampling: ImageSampling,
    },
    ProgressBar {
        value: u32,
        label: String,
        color: Option<Color32>,
    },
    /// Label with explicit foreground color (bypasses the theme).
    StyledLabel {
        text: String,
        color: Color32,
        alignment: TextAlignment,
    },

    ScrollView {
        child: Box<Node<M>>,
        direction: ScrollDirection,
        show_scrollbar: ScrollbarVisibility,
        /// Initial scroll offset (preserved across rebuilds).
        scroll_y: i32,
        /// Emitted with the new `offset_y` whenever it changes.
        on_scroll: Option<fn(i32) -> M>,
    },
    ListView {
        item_height: i32,
        selected: Option<usize>,
        on_select: Option<fn(usize) -> M>,
        items: Vec<Node<M>>,
    },
    TabBar {
        tabs: Vec<String>,
        active: usize,
        on_change: Option<fn(usize) -> M>,
        content: Vec<Node<M>>,
    },
    Menu {
        items: Vec<MenuItem>,
        on_action: Option<fn(usize) -> M>,
    },
    Table {
        columns: Vec<TableColumn>,
        rows: Vec<Vec<Node<M>>>,
        row_height: i32,
        selected: Option<usize>,
        on_select: Option<fn(usize) -> M>,
        /// `None` leaves headers unclickable; `Some` emits with the column index.
        on_header_click: Option<fn(usize) -> M>,
        /// Emitted on secondary click and on Menu / Shift+F10, after selection
        /// has moved to the row.
        on_context_menu: Option<fn(ContextMenuAt) -> M>,
    },
    Dialog {
        title: String,
        content: Box<Node<M>>,
        actions: Vec<Node<M>>,
        on_dismiss: Option<M>,
    },
    /// Child floated at an absolute window position, clamped on-screen. A click
    /// outside it or an Escape press emits `on_dismiss`, leaving the app's own
    /// state the single source of truth for whether the popup is open.
    Popup {
        x: i32,
        y: i32,
        child: Box<Node<M>>,
        on_dismiss: Option<M>,
    },

    VStack {
        children: Vec<Node<M>>,
        spacing: i32,
        align: CrossAxisAlignment,
    },
    HStack {
        children: Vec<Node<M>>,
        spacing: i32,
        align: CrossAxisAlignment,
    },
    ZStack {
        children: Vec<Node<M>>,
    },
    Padding {
        padding: EdgeInsets,
        child: Box<Node<M>>,
    },
    Spacer {
        size: Length,
    },
    Expand {
        weight: u16,
        child: Box<Node<M>>,
    },

    Background {
        color: Color32,
        child: Box<Node<M>>,
    },
    SizedBox {
        width: Option<Length>,
        height: Option<Length>,
        child: Box<Node<M>>,
    },

    Canvas {
        width: i32,
        height: i32,
    },

    Empty,
}

pub trait App {
    type Message: Clone + 'static;

    fn view(&self) -> Node<Self::Message>;

    fn update(&mut self, msg: Self::Message) -> Action;

    /// Return `Some(ms)` to receive [`App::tick`] calls at that interval.
    fn tick_interval_ms(&self) -> Option<u64> {
        None
    }

    fn tick(&mut self) -> Action {
        Action::None
    }

    /// Called when a key event is not consumed by any widget.
    fn on_key(&mut self, _key: Key, _modifiers: Modifiers) -> Action {
        Action::None
    }

    /// Read once at startup.
    fn title(&self) -> &str {
        "SlopOS App"
    }

    /// App ID for the compositor (e.g. "org.slopos.sysmon").
    fn app_id(&self) -> &str {
        ""
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Action {
    None,
    /// Rebuild the widget tree on the next frame.
    Rebuild,
    Exit,
}
