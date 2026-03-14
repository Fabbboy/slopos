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
pub mod run;
pub mod surface;
pub mod window;

pub use event::Event;
pub use run::{run, ControlFlow, WindowedApp};
pub use surface::{Surface, SurfaceError};
pub use window::Window;

/// Generate a minimal `_start` entry point and panic handler for a
/// userland binary.
///
/// The provided function must have signature `fn(*mut T) -> !` or
/// `fn(*mut T)` (the latter will cleanly exit after returning).
///
/// If the application returns, the process exits with code 0 via
/// `sys_exit`.
///
/// # Stack Alignment
///
/// At ELF entry the SysV ABI guarantees `rsp` is 16-byte aligned with
/// `[rsp] = argc`.  A compiler-generated `extern "C"` function assumes it
/// was *called* (`rsp` 8-mod-16 due to the pushed return address).
/// We use a naked stub so the `call` instruction itself pushes a return
/// address and gives the callee the expected 8-mod-16 alignment.
///
/// # Usage
///
/// ```rust,ignore
/// #![no_main]
/// slopos_userland::entry!(slopos_userland::apps::file_manager::file_manager_main);
/// ```
#[macro_export]
macro_rules! entry {
    ($main_fn:path) => {
        /// Rust trampoline called from naked `_start`.
        /// Invokes the application entry point then exits cleanly.
        #[allow(unreachable_code)]
        #[inline(never)]
        extern "C" fn __slopos_entry() -> ! {
            $main_fn(core::ptr::null_mut());
            $crate::syscall::core::exit();
        }

        /// ELF entry point (naked).
        ///
        /// `rsp` is 16-byte aligned at entry.  `call` pushes a return
        /// address so the callee sees 8-mod-16 — matching the C ABI.
        #[unsafe(no_mangle)]
        #[unsafe(naked)]
        unsafe extern "C" fn _start() -> ! {
            core::arch::naked_asm!(
                "xor ebp, ebp",  // mark end of call chain
                "call {}",
                "ud2",
                sym __slopos_entry,
            );
        }
    };
}
