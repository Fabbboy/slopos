//! DHCP: the wire format, and the client that speaks it.
//!
//! Split so the two halves can be reasoned about separately. [`codec`] is pure
//! encode/decode; [`client`] is a pure state machine over events and actions.
//! Neither touches a socket, a timer or a clock, which is what lets a test walk
//! a lease from DISCOVER through renewal, rebinding and expiry in a few
//! microseconds of `MockClock` time.
//!
//! The transport that binds this to a real interface — a UDP port listener on
//! 68, the timer arming, and installing what a lease says — is wired on top of
//! these two and is deliberately the only part that knows about either.

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
