//! Backward-compatibility re-exports from slopos-windowing.
//!
//! New code should depend on `slopos-windowing` directly.
//! These re-exports exist so that `slopos_appkit::platform::*`
//! paths continue to resolve during the migration period.

pub mod protocol_client {
    pub use slopos_windowing::connection::*;
}

pub mod surface {
    pub use slopos_windowing::surface::*;
}

pub mod window {
    pub use slopos_windowing::window::*;
}

pub mod event {
    pub use slopos_windowing::event::*;
}
