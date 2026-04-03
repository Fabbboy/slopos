use slopos_abi::draw::Color32;

use super::constraints::{
    CrossAxisAlignment, EdgeInsets, ImageScale, Length, ScrollDirection, ScrollbarVisibility,
    TextAlignment,
};
use super::event::{Key, Modifiers};

/// Message identifier with payload for routing widget actions to the App.
/// `id` identifies the action type, `payload` carries context (e.g. which tab, row, column).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct MessageId {
    pub id: u32,
    pub payload: u32,
}

impl MessageId {
    pub const fn new(id: u32) -> Self {
        Self { id, payload: 0 }
    }

    pub const fn with_payload(id: u32, payload: u32) -> Self {
        Self { id, payload }
    }
}

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

/// Table column definition.
#[derive(Clone, Debug)]
pub struct TableColumn {
    pub label: String,
    pub width: TableColumnWidth,
    pub sort_indicator: Option<SortIndicator>,
}

/// Declarative tree description. Apps build a tree of Nodes in `view()`.
/// The framework diffs against the previous tree to update retained widgets.
#[derive(Clone, Debug)]
pub enum Node {
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
        on_press: Option<MessageId>,
        style: ButtonStyle,
        enabled: bool,
    },
    TextField {
        text: String,
        placeholder: String,
        on_change: MessageId,
        max_length: Option<usize>,
        read_only: bool,
    },
    Checkbox {
        checked: bool,
        label: String,
        on_toggle: MessageId,
        enabled: bool,
    },
    /// Divider line. Automatically detects orientation from parent layout context:
    /// horizontal in VStack, vertical in HStack.
    Divider,
    Image {
        width: u32,
        height: u32,
        scale: ImageScale,
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
        child: Box<Node>,
        direction: ScrollDirection,
        show_scrollbar: ScrollbarVisibility,
        /// Initial scroll offset (preserved across rebuilds).
        scroll_y: i32,
        /// Emitted when scroll offset changes (payload = new offset_y).
        on_scroll: Option<MessageId>,
    },
    ListView {
        item_height: i32,
        selected: Option<usize>,
        on_select: MessageId,
        items: Vec<Node>,
    },
    TabBar {
        tabs: Vec<String>,
        active: usize,
        on_change: MessageId,
        content: Vec<Node>,
    },
    Menu {
        items: Vec<MenuItem>,
        on_action: MessageId,
    },
    Table {
        columns: Vec<TableColumn>,
        rows: Vec<Vec<Node>>,
        row_height: i32,
        selected: Option<usize>,
        on_select: MessageId,
        /// None = headers not clickable. Some = emits with column index as payload.
        on_header_click: Option<MessageId>,
    },
    Dialog {
        title: String,
        content: Box<Node>,
        actions: Vec<Node>,
        on_dismiss: MessageId,
    },

    // --- Layout containers ---
    VStack {
        children: Vec<Node>,
        spacing: i32,
        align: CrossAxisAlignment,
    },
    HStack {
        children: Vec<Node>,
        spacing: i32,
        align: CrossAxisAlignment,
    },
    ZStack {
        children: Vec<Node>,
    },
    Padding {
        padding: EdgeInsets,
        child: Box<Node>,
    },
    Spacer {
        size: Length,
    },
    Expand {
        weight: u16,
        child: Box<Node>,
    },

    /// Container that paints a solid background color behind its child.
    Background {
        color: Color32,
        child: Box<Node>,
    },
    /// Container with explicit width and/or height constraints.
    SizedBox {
        width: Option<Length>,
        height: Option<Length>,
        child: Box<Node>,
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
    type Message: From<MessageId>;

    /// Build the widget tree. Called when the tree needs to be (re)constructed.
    fn view(&self) -> Node;

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
