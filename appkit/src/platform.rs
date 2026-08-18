//! Backward-compatibility re-exports so `slopos_appkit::platform::*` paths keep
//! resolving; new code should depend on `slopos-windowing` directly.

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
