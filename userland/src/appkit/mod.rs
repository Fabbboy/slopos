//! AppKit — SlopOS application toolkit.
//!
//! Provides everything needed to build GUI applications:
//!
//! - **Widget apps** (primary): Implement [`App`], return a [`Node`] tree from
//!   `view()`, handle messages in `update()`. Call [`run_app()`] to launch.
//!
//! - **Raw-drawing apps** (escape hatch): Implement [`raw::WindowedApp`],
//!   draw into a [`DrawBuffer`](crate::gfx::DrawBuffer) directly.
//!   Call [`raw::run()`] to launch.
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use crate::appkit::{App, Action, Node, MessageId, run_app};
//!
//! struct MyApp;
//! impl App for MyApp {
//!     type Message = MyMsg;
//!     fn view(&self) -> Node { Node::Label { text: "Hello".into(), .. } }
//!     fn update(&mut self, msg: MyMsg) -> Action { Action::None }
//! }
//!
//! pub fn main() -> ! { run_app(MyApp, 640, 480) }
//! ```

// Widget toolkit — many fields are read by the framework integration
// layer (run_app, tree reconciliation) rather than within widget impls.
#![allow(dead_code)]

// === Platform internals (not part of public API) ===
pub(crate) mod platform;

// === Raw drawing escape hatch ===
pub mod raw;

// === Widget toolkit modules ===
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

// === Public re-exports: primary app API ===
pub use node::{
    Action, App, ButtonStyle, MenuItem, MenuItemKind, MessageId, Node, SortIndicator, TableColumn,
    TableColumnWidth,
};
pub use run::run_app;

// === Public re-exports: layout & constraint types ===
pub use constraints::{
    BoxConstraints, CrossAxisAlignment, EdgeInsets, ImageScale, Length, Orientation, Rect,
    ScrollDirection, ScrollbarVisibility, Size, SizePolicy, TextAlignment,
};

// === Public re-exports: event & input types ===
pub use event::{
    EventPhase, EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent,
};

// === Public re-exports: framework types ===
pub use dirty::DirtyFlags;
pub use focus::FocusManager;
pub use paint::PaintContext;
pub use style::StyleSheet;
pub use traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId};

// === Public re-exports: raw drawing escape hatch ===
pub use raw::{ControlFlow, WindowedApp};

// === Public re-exports: protocol types for threading ===
pub use platform::protocol_client::{ProtocolHandle, UiSender};
