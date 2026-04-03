//! slopos-windowing — compositor connection, surface management, and event loop.
//!
//! Provides everything needed to create a window, get a pixel buffer,
//! receive input events, and run an event loop — without pulling in
//! the widget toolkit.
//!
//! # Quick start (raw drawing)
//!
//! ```rust,ignore
//! use slopos_windowing::{WindowedApp, Window, ControlFlow, run};
//! use slopos_gfx::DrawBuffer;
//!
//! struct MyApp;
//! impl WindowedApp for MyApp {
//!     fn draw(&mut self, fb: &mut DrawBuffer<'_>) { /* ... */ }
//! }
//!
//! pub fn main() -> ! { run(MyApp, 640, 480) }
//! ```

#![feature(restricted_std)]
#![allow(dead_code)]

pub mod app;
pub mod connection;
pub mod event;
pub(crate) mod shm;
pub mod surface;
pub(crate) mod sys;
pub mod window;

// Flat re-exports for ergonomic use.
pub use app::{ControlFlow, WindowedApp, run};
pub use connection::{Protocol, ProtocolHandle, UiSender, connect};
pub use event::Event;
pub use surface::{Surface, SurfaceError};
pub use window::{EVENT_BUF_LEN, Window};

/// Get monotonic time in milliseconds.
#[inline]
pub fn get_time_ms() -> u64 {
    sys::get_time_ms()
}
