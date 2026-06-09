#![no_std]
#![forbid(unsafe_code)]

pub mod blend;
pub mod canvas_ops;
pub mod damage;
pub mod draw_buffer;
pub mod image;
pub mod render_surface;

pub use damage::{DamageTracker, InternalDamageTracker};
pub use draw_buffer::DrawBuffer;
pub use render_surface::{RenderError, RenderSurface};

#[cfg(feature = "alloc")]
pub use render_surface::HeadlessSurface;
