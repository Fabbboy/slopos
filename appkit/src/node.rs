use slopos_abi::draw::Color32;
use slopos_gfx::image::ImageSampling;
use std::sync::Arc;

use super::constraints::{
    CrossAxisAlignment, EdgeInsets, ImageScale, Length, ScrollDirection, ScrollbarVisibility,
    TextAlignment,
};
use super::event::{Key, Modifiers};

/// Button visual style.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ButtonStyle {
    #[default]
    Primary,
    Secondary,
    Destructive,
}

/// Menu item definition.
#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: &'static str,
    pub shortcut: Option<&'static str>,
    pub enabled: bool,
    pub kind: MenuItemKind,
}

/// Menu item kind.
#[derive(Clone, Debug)]
pub enum MenuItemKind {
    Action,
    Separator,
    Submenu(Vec<MenuItem>),
}

/// Table column width specification.
#[derive(Copy, Clone, Debug)]
pub enum TableColumnWidth {
    /// Fixed pixel width.
    Fixed(i32),
    /// Proportional flex weight.
    Flex(u16),
}

/// Sort indicator for table column headers.
#[derive(Copy, Clone, Debug)]
pub enum SortIndicator {
    Ascending,
    Descending,
}

/// Where a context-menu request landed.
///
/// `x`/`y` are window coordinates. For a keyboard-raised request (Menu key,
/// Shift+F10) they are the selected row's left edge, so a popup anchored to
/// them appears in the same place a pointer-raised one would.
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

/// Table column definition.
#[derive(Clone, Debug)]
pub struct TableColumn {
    pub label: String,
    pub width: TableColumnWidth,
    pub sort_indicator: Option<SortIndicator>,
}

/// Declarative tree description. Apps build a tree of `Node<M>` in `view()`.
/// The framework diffs against the previous tree to update retained widgets.
/// `M` is the application's message type — widgets emit concrete `M` values
/// instead of opaque integer IDs.
pub enum Node<M> {
    // --- Leaf widgets ---
    Label {
        text: String,
        alignment: TextAlignment,
        wrap: bool,
        max_lines: Option<u32>,
    },
    Button {
        label: String,
        /// None = no action (button is non-interactive). Some = emits this message on click.
        /// This makes silent no-ops impossible — if you want a clickable button, you MUST
        /// provide a message. If you don't want an action, use None explicitly.
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
    /// Divider line. Automatically detects orientation from parent layout context:
    /// horizontal in VStack, vertical in HStack.
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

    // --- Container widgets ---
    ScrollView {
        child: Box<Node<M>>,
        direction: ScrollDirection,
        show_scrollbar: ScrollbarVisibility,
        /// Initial scroll offset (preserved across rebuilds).
        scroll_y: i32,
        /// Emitted when scroll offset changes (argument = new offset_y).
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
        /// None = headers not clickable. Some = emits with column index as argument.
        on_header_click: Option<fn(usize) -> M>,
        /// None = rows have no context menu. Some = emits on secondary click and
        /// on the Menu / Shift+F10 keys, after moving selection to the row.
        on_context_menu: Option<fn(ContextMenuAt) -> M>,
    },
    Dialog {
        title: String,
        content: Box<Node<M>>,
        actions: Vec<Node<M>>,
        on_dismiss: Option<M>,
    },
    /// Child floated at an absolute window position, over the rest of the
    /// parent's area. Clamped to stay on-screen. A click outside the child or
    /// an Escape press emits `on_dismiss`, so the owning app's state stays the
    /// single source of truth for whether the popup is open.
    Popup {
        x: i32,
        y: i32,
        child: Box<Node<M>>,
        on_dismiss: Option<M>,
    },

    // --- Layout containers ---
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

    /// Container that paints a solid background color behind its child.
    Background {
        color: Color32,
        child: Box<Node<M>>,
    },
    /// Container with explicit width and/or height constraints.
    SizedBox {
        width: Option<Length>,
        height: Option<Length>,
        child: Box<Node<M>>,
    },

    /// Escape hatch: raw drawing callback.
    Canvas {
        width: i32,
        height: i32,
    },

    /// Empty placeholder.
    Empty,
}

/// Application trait driven by the widget framework.
pub trait App {
    /// The message type for this application's events.
    type Message: Clone + 'static;

    /// Build the widget tree. Called when the tree needs to be (re)constructed.
    fn view(&self) -> Node<Self::Message>;

    /// Handle a widget event. Return an Action indicating what happened.
    fn update(&mut self, msg: Self::Message) -> Action;

    /// Optional periodic tick interval in milliseconds.
    /// Return Some(ms) to receive tick() calls at that interval.
    fn tick_interval_ms(&self) -> Option<u64> {
        None
    }

    /// Called periodically if tick_interval_ms() returns Some.
    fn tick(&mut self) -> Action {
        Action::None
    }

    /// Called when a key event is not consumed by any widget.
    fn on_key(&mut self, _key: Key, _modifiers: Modifiers) -> Action {
        Action::None
    }

    /// Window title. Called once at startup.
    fn title(&self) -> &str {
        "SlopOS App"
    }

    /// App ID for the compositor (e.g. "org.slopos.sysmon").
    fn app_id(&self) -> &str {
        ""
    }
}

/// Action returned from App::update().
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Nothing changed; no rebuild needed.
    None,
    /// State changed; rebuild the widget tree on next frame.
    Rebuild,
    /// Exit the application.
    Exit,
}
