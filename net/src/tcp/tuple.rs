//! TCP connection identity and error type.
//!
//! [`TcpTuple`] is the four-tuple that uniquely identifies a connection on
//! the wire (and together with the TCB state decides how an incoming segment
//! is routed).  [`TcpError`] is the error type every TCP lifecycle operation
//! returns.  Both types live in their own module so the state machine in
//! `tcp/pcb/` can depend on them without pulling in anything from the main
//! `tcp/mod.rs` file.

/// Four-tuple identifying a TCP connection.
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

    /// Check if this tuple matches a specific remote endpoint (for listen
    /// sockets, `remote_ip`/`remote_port` may be zero = wildcard).
    pub fn matches(&self, other: &TcpTuple) -> bool {
        self.local_ip == other.local_ip
            && self.local_port == other.local_port
            && (self.remote_ip == [0; 4] || self.remote_ip == other.remote_ip)
            && (self.remote_port == 0 || self.remote_port == other.remote_port)
    }
}

/// Error type for TCP operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpError {
    /// Connection table is full.
    TableFull,
    /// No connection found for the given tuple.
    NotFound,
    /// Connection is in wrong state for the requested operation.
    InvalidState,
    /// Port already in use.
    AddrInUse,
    /// Connection was reset by peer.
    ConnectionReset,
    /// Connection timed out.
    TimedOut,
    /// Connection refused by peer (RST received in SYN_SENT).
    ConnectionRefused,
    /// Invalid segment or parameter.
    InvalidSegment,
    /// Heap allocation failure during a state transition.
    OutOfMemory,
}

impl From<slopos_alloc::AllocError> for TcpError {
    fn from(_: slopos_alloc::AllocError) -> Self {
        TcpError::OutOfMemory
    }
}
