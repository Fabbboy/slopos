// Widget toolkit is a library — many fields are read by the framework integration
// layer (run_app, tree reconciliation) rather than within the widget impls.
#![allow(dead_code)]

pub mod constraints;
pub mod dirty;
pub mod event;
pub mod focus;
pub mod input;
pub mod layout;
pub mod node;
pub mod overlay;
pub mod paint;
pub mod run;
pub mod style;
pub mod tests;
pub mod traits;
pub mod tree;
pub mod widgets;

// Public re-exports for convenience.
pub use constraints::{
    BoxConstraints, CrossAxisAlignment, EdgeInsets, ImageScale, Length, Orientation, Rect,
    ScrollDirection, ScrollbarVisibility, Size, SizePolicy, TextAlignment,
};
pub use dirty::DirtyFlags;
pub use event::{
    EventPhase, EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent,
};
pub use focus::FocusManager;
pub use node::{
    Action, App, ButtonStyle, MenuItem, MenuItemKind, MessageId, Node, SortIndicator, TableColumn,
    TableColumnWidth,
};
pub use paint::PaintContext;
pub use run::run_app;
pub use style::StyleSheet;
pub use traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId};
