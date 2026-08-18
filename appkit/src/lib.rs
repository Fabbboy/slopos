//! AppKit — SlopOS application toolkit.
//!
//! Widget apps implement [`App`], return a [`Node`] tree from `view()`, handle
//! messages in `update()`, and launch with [`run_app()`]. Raw-drawing apps use
//! `slopos-windowing` directly via [`WindowedApp`].
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
// Widget fields are read by run_app and tree reconciliation, not by the impls.
#![allow(dead_code)]

pub mod platform;
pub mod text;

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

pub use node::{
    Action, App, ButtonStyle, ContextMenuAt, ImageData, MenuItem, MenuItemKind, Node,
    SortIndicator, TableColumn, TableColumnWidth,
};
pub use run::run_app;

pub use constraints::{
    BoxConstraints, CrossAxisAlignment, EdgeInsets, ImageScale, Length, Orientation, Rect,
    ScrollDirection, ScrollbarVisibility, Size, SizePolicy, TextAlignment,
};

pub use event::{
    EventPhase, EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent,
};

pub use dirty::DirtyFlags;
pub use focus::FocusManager;
pub use paint::PaintContext;
pub use style::StyleSheet;
pub use traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId};

pub use slopos_windowing::{ControlFlow, WindowedApp};
pub use slopos_windowing::{ProtocolHandle, UiSender};

pub use slopos_gfx::image::ImageSampling;
pub use slopos_gfx::{RenderError, RenderSurface};
