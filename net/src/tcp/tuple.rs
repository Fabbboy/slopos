//! TCP connection identity and error type.
//!
//! Both live outside `tcp/mod.rs` so the state machine in `tcp/pcb/` can depend
//! on them without pulling the rest of that module in.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpTuple {
    pub local_ip: [u8; 4],
    pub local_port: u16,
    pub remote_ip: [u8; 4],
    pub remote_port: u16,
}

impl TcpTuple {
    pub const ZERO: Self = Self {
        local_ip: [0; 4],
        local_port: 0,
        remote_ip: [0; 4],
        remote_port: 0,
    };

    /// A zero `remote_ip`/`remote_port` is a wildcard, as on a listen socket.
    pub fn matches(&self, other: &TcpTuple) -> bool {
        self.local_ip == other.local_ip
            && self.local_port == other.local_port
            && (self.remote_ip == [0; 4] || self.remote_ip == other.remote_ip)
            && (self.remote_port == 0 || self.remote_port == other.remote_port)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpError {
    TableFull,
    NotFound,
    InvalidState,
    AddrInUse,
    ConnectionReset,
    TimedOut,
    /// RST received in SYN_SENT.
    ConnectionRefused,
    InvalidSegment,
    OutOfMemory,
}

impl slopos_abi::KernelErrno for TcpError {
    fn to_errno(&self) -> i32 {
        use slopos_abi::syscall::*;
        let code = match self {
            Self::NotFound => ERRNO_ENOTSOCK,
            Self::InvalidState => ERRNO_ENOTCONN,
            Self::AddrInUse => ERRNO_EADDRINUSE,
            Self::TableFull => ERRNO_ENOMEM,
            Self::ConnectionRefused => ERRNO_ECONNREFUSED,
            Self::ConnectionReset => ERRNO_ECONNRESET,
            Self::TimedOut => ERRNO_EAGAIN,
            Self::InvalidSegment => ERRNO_EINVAL,
            Self::OutOfMemory => ERRNO_ENOMEM,
        };
        code as i32
    }
}

impl From<slopos_ostd::AllocError> for TcpError {
    fn from(_: slopos_ostd::AllocError) -> Self {
        TcpError::OutOfMemory
    }
}
