//! Which socket rows `ss`'s flags select.
//!
//! Pure predicate, no I/O, so the flag algebra is host-testable — `ss` itself
//! lives in `userland`, where `test = false` means every `#[cfg(test)]` is dead
//! code that never compiles.
//!
//! Ownership is deliberately not filtered here: every row is visible to every
//! caller, and what the kernel withholds is the `owner_pid` attribution, by
//! writing a sentinel into a field this predicate never reads.

use slopos_abi::net::{NET_SOCK_CLOSED, NET_SOCK_LISTEN, NET_SOCK_UNCONN, SOCK_DGRAM, SOCK_STREAM};

/// `-t`: TCP rows.
pub const SS_TCP: u32 = 1 << 0;
/// `-u`: UDP rows.
pub const SS_UDP: u32 = 1 << 1;
/// `-l`: listening sockets only.
pub const SS_LISTEN: u32 = 1 << 2;
/// `-a`: every socket, whatever its state.
pub const SS_ALL: u32 = 1 << 3;

/// Whether a socket of `sock_type` in `state` survives `opts`.
///
/// Transport and state are two independent decisions, in that order.
///
/// Naming no transport means every transport, and naming several means the
/// union — `-tu` is "TCP or UDP".
///
/// `-a` outranks `-l`, asking for everything and for a subset at once being
/// best read as the wider request. With neither, only *connected* sockets are
/// listed: that default is `ss`'s own and surprises people who expect it to
/// mean "show me sockets", which is what `-a` is for.
pub fn ss_row_selected(opts: u32, sock_type: u8, state: u8) -> bool {
    let want_tcp = opts & SS_TCP != 0;
    let want_udp = opts & SS_UDP != 0;
    if want_tcp || want_udp {
        let is_tcp = sock_type == SOCK_STREAM as u8;
        let is_udp = sock_type == SOCK_DGRAM as u8;
        if !((want_tcp && is_tcp) || (want_udp && is_udp)) {
            return false;
        }
    }

    if opts & SS_ALL != 0 {
        return true;
    }
    if opts & SS_LISTEN != 0 {
        return state == NET_SOCK_LISTEN;
    }
    is_connected(state)
}

/// Whether a state means "there is a peer at the other end".
///
/// Named rather than inlined because `ss`'s default view and its `-s` summary
/// must agree about what "connected" means.
pub const fn is_connected(state: u8) -> bool {
    state != NET_SOCK_LISTEN && state != NET_SOCK_CLOSED && state != NET_SOCK_UNCONN
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopos_abi::net::{NET_SOCK_ESTABLISHED, NET_SOCK_TIME_WAIT};

    const TCP: u8 = SOCK_STREAM as u8;
    const UDP: u8 = SOCK_DGRAM as u8;

    #[test]
    fn a_bare_ss_shows_connected_sockets_only() {
        assert!(ss_row_selected(0, TCP, NET_SOCK_ESTABLISHED));
        assert!(ss_row_selected(0, TCP, NET_SOCK_TIME_WAIT));
        assert!(!ss_row_selected(0, TCP, NET_SOCK_LISTEN));
        assert!(!ss_row_selected(0, UDP, NET_SOCK_UNCONN));
        assert!(!ss_row_selected(0, TCP, NET_SOCK_CLOSED));
    }

    #[test]
    fn listen_selects_only_listening() {
        assert!(ss_row_selected(SS_LISTEN, TCP, NET_SOCK_LISTEN));
        assert!(!ss_row_selected(SS_LISTEN, TCP, NET_SOCK_ESTABLISHED));
        assert!(!ss_row_selected(SS_LISTEN, UDP, NET_SOCK_UNCONN));
    }

    #[test]
    fn all_selects_every_state() {
        for state in 0u8..=12 {
            assert!(ss_row_selected(SS_ALL, TCP, state), "state {state}");
            assert!(ss_row_selected(SS_ALL, UDP, state), "state {state}");
        }
    }

    #[test]
    fn all_outranks_listen() {
        assert!(ss_row_selected(
            SS_ALL | SS_LISTEN,
            TCP,
            NET_SOCK_ESTABLISHED
        ));
        assert!(ss_row_selected(SS_ALL | SS_LISTEN, UDP, NET_SOCK_UNCONN));
    }

    #[test]
    fn transport_flags_union_rather_than_intersect() {
        assert!(ss_row_selected(
            SS_TCP | SS_UDP | SS_ALL,
            TCP,
            NET_SOCK_LISTEN
        ));
        assert!(ss_row_selected(
            SS_TCP | SS_UDP | SS_ALL,
            UDP,
            NET_SOCK_UNCONN
        ));
        assert!(!ss_row_selected(SS_TCP | SS_ALL, UDP, NET_SOCK_UNCONN));
        assert!(!ss_row_selected(SS_UDP | SS_ALL, TCP, NET_SOCK_LISTEN));
    }

    #[test]
    fn no_transport_flag_means_every_transport() {
        assert!(ss_row_selected(SS_ALL, TCP, NET_SOCK_LISTEN));
        assert!(ss_row_selected(SS_ALL, UDP, NET_SOCK_UNCONN));
        // Including a transport the flags cannot name, such as raw or icmp.
        assert!(ss_row_selected(SS_ALL, 99, NET_SOCK_UNCONN));
    }

    #[test]
    fn transport_and_state_filters_are_independent() {
        assert!(!ss_row_selected(SS_TCP, TCP, NET_SOCK_LISTEN));
        assert!(ss_row_selected(SS_TCP | SS_LISTEN, TCP, NET_SOCK_LISTEN));
        assert!(!ss_row_selected(SS_UDP | SS_LISTEN, TCP, NET_SOCK_LISTEN));
    }

    #[test]
    fn connected_excludes_exactly_three_states() {
        for state in 0u8..=12 {
            let want =
                state != NET_SOCK_LISTEN && state != NET_SOCK_CLOSED && state != NET_SOCK_UNCONN;
            assert_eq!(is_connected(state), want, "state {state}");
        }
    }
}
