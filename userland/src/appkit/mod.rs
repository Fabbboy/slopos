//! Application framework for SlopOS windowed applications.
//!
//! Provides `Surface`, `Window`, `Event`, and a generic `run()` loop that
//! eliminate the boilerplate of surface creation, pixel format negotiation,
//! event polling, and frame presentation.
//!
//! # Example
//!
//! ```rust,ignore
//! use crate::appkit::{self, ControlFlow, Event, Window, WindowedApp};
//! use crate::gfx::DrawBuffer;
//!
//! struct MyApp;
//!
//! impl WindowedApp for MyApp {
//!     fn init(&mut self, win: &mut Window) {
//!         win.set_title("My App");
//!         win.request_redraw();
//!     }
//!
//!     fn draw(&mut self, fb: &mut DrawBuffer<'_>) {
//!         // render here
//!     }
//! }
//!
//! pub fn main() -> ! {
//!     appkit::run(MyApp, 640, 480)
//! }
//! ```

pub mod event;
pub mod protocol_client;
pub mod run;
pub mod surface;
pub mod window;

pub use event::Event;
pub use run::{ControlFlow, WindowedApp, run};
pub use surface::{Surface, SurfaceError};
pub use window::Window;
