//! AppKit — SlopOS application toolkit.
//!
//! Provides everything needed to build widget-based GUI applications:
//!
//! - **Widget apps** (primary): Implement [`App`], return a [`Node`] tree from
//!   `view()`, handle messages in `update()`. Call [`run_app()`] to launch.
//!
//! - **Raw-drawing apps**: Use `slopos-windowing` directly — implement
//!   [`WindowedApp`], draw into a [`DrawBuffer`](slopos_gfx::DrawBuffer),
//!   and call [`slopos_windowing::run()`].
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use slopos_appkit::{App, Action, Node, run_app};
//!
//! struct MyApp;
//! impl App for MyApp {
//!     type Message = MyMsg;
//!     fn view(&self) -> Node<MyMsg> { Node::Label { text: "Hello".into(), .. } }
//!     fn update(&mut self, msg: MyMsg) -> Action { Action::None }
//! }
//!
//! pub fn main() -> ! { run_app(MyApp, 640, 480) }
//! ```

#![feature(restricted_std)]
// Widget toolkit — many fields are read by the framework integration
// layer (run_app, tree reconciliation) rather than within widget impls.
#![allow(dead_code)]

// === Platform layer (backward-compat re-exports from slopos-windowing) ===
pub mod platform;

// === Text rendering ===
pub mod text;

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
    Action, App, ButtonStyle, MenuItem, MenuItemKind, Node, SortIndicator, TableColumn,
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

// === Public re-exports: windowing types ===
pub use slopos_windowing::{ControlFlow, WindowedApp};
pub use slopos_windowing::{ProtocolHandle, UiSender};

// === Public re-exports: render surface abstraction ===
pub use slopos_gfx::{RenderError, RenderSurface};
