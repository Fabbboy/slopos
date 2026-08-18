//! DHCP: the wire format, and the client that speaks it.
//!
//! [`codec`] is pure encode/decode and [`client`] a pure state machine over
//! events and actions; neither touches a socket, a timer or a clock, so a test
//! can walk a lease through renewal and expiry on `MockClock` time. [`transport`]
//! is the only half that knows about the interface, the port and the timers.

pub mod client;
pub mod codec;
pub mod transport;

pub use client::{
    DhcpAction, DhcpBinding, DhcpClient, DhcpDest, DhcpEvent, DhcpState, UnbindReason,
};
pub use codec::{
    BOOTP_HEADER_LEN, DHCP_FRAME_LEN, DhcpLease, DhcpReply, MSG_ACK, MSG_DECLINE, MSG_DISCOVER,
    MSG_NAK, MSG_OFFER, MSG_RELEASE, MSG_REQUEST, UDP_PORT_CLIENT, UDP_PORT_SERVER, build_discover,
    build_release, build_request_reboot, build_request_renew, build_request_selecting, parse_reply,
};

pub use transport::{
    init, is_running, lease_of, on_carrier, on_expire_timer, on_retransmit_timer, on_t1_timer,
    on_t2_timer, renew_now, start, state_of, stop, stop_with,
};
