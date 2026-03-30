//! ABI types for AF_UNIX (Unix domain) sockets.

/// Maximum path length for Unix socket abstract namespace addresses.
pub const UNIX_PATH_MAX: usize = 108;

/// Unix domain socket address — mirrors POSIX `sockaddr_un` layout.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SockAddrUn {
    pub family: u16,
    pub path: [u8; UNIX_PATH_MAX],
}

impl Default for SockAddrUn {
    fn default() -> Self {
        Self {
            family: 0,
            path: [0u8; UNIX_PATH_MAX],
        }
    }
}

const _: () = assert!(
    core::mem::size_of::<SockAddrUn>() == 110,
    "SockAddrUn must be exactly 110 bytes"
);
