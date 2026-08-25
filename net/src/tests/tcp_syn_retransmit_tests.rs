//! Active-open SYN retransmission (RFC 793 §3.4, RFC 6298 §5).
//!
//! `SynSentState` has carried `retransmits` and `retransmit_token` since it was
//! written, and nothing incremented either: `connect` armed no timer and
//! `on_retransmit` matched only `PcbState::Data`. A SYN was therefore sent
//! exactly once, so one lost SYN stalled the connect for its whole wall-clock
//! deadline and an unreachable peer for all of it.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use super::net_scope::NetTestScope;
use super::tcp_common::{LOCAL_IP, REMOTE_IP, REMOTE_PORT};
use crate::tcp::header::{TCP_FLAG_ACK, TCP_FLAG_SYN};
use crate::tcp::{self, ACTIVE_SYN_RETRIES_MAX, RetransmitAction};

fn open(scope_ms: u64) -> Result<(NetTestScope, tcp::ConnId, u32), &'static str> {
    let scope = NetTestScope::enter_at_mock_ms(scope_ms).map_err(|_| "net scope")?;
    let (id, syn) = tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT).map_err(|_| "connect")?;
    tcp::arm_syn_retransmit(id);
    Ok((scope, id, syn.seq_num))
}

fn pcb_is_live(id: tcp::ConnId) -> bool {
    tcp::with_pcb(id, |_| ()).is_some()
}

/// Every retransmit carries the original ISS: a SYN re-sent under a new
/// sequence number is a second connection attempt, and the peer's SYN-ACK for
/// the first would then be rejected.
pub fn test_syn_is_retransmitted_with_the_original_iss() -> TestResult {
    let (_scope, id, iss) = match open(100) {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };

    match tcp::on_retransmit(id.raw()) {
        RetransmitAction::Segment(seg) => {
            assert_eq_test!(
                seg.flags & (TCP_FLAG_SYN | TCP_FLAG_ACK),
                TCP_FLAG_SYN,
                "a retransmitted active-open segment must be a bare SYN"
            );
            assert_eq_test!(seg.seq_num, iss, "retransmitted SYN changed the ISS");
        }
        RetransmitAction::Data(_) => {
            return fail!("a SynSent PCB was routed to the data retransmit path");
        }
        RetransmitAction::Nothing => {
            return fail!("the SYN was not retransmitted -- connect sends it exactly once");
        }
    }
    assert_test!(pcb_is_live(id), "a first retransmit dropped the connection");
    pass!()
}

/// The attempt is abandoned rather than retried forever, and the PCB goes with
/// it: a `SynSent` entry nothing retires holds its shard slot and its ephemeral
/// port for the life of the boot.
pub fn test_syn_retransmits_are_bounded_and_release_the_pcb() -> TestResult {
    let (_scope, id, _iss) = match open(200) {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };

    for attempt in 1..=ACTIVE_SYN_RETRIES_MAX {
        match tcp::on_retransmit(id.raw()) {
            RetransmitAction::Segment(_) => {}
            _ => {
                return fail!(
                    "retransmit {} of {} did not re-send the SYN",
                    attempt,
                    ACTIVE_SYN_RETRIES_MAX
                );
            }
        }
    }

    match tcp::on_retransmit(id.raw()) {
        RetransmitAction::Nothing => {}
        RetransmitAction::Segment(_) => {
            return fail!(
                "a {}th retransmit was sent -- the attempt is unbounded",
                ACTIVE_SYN_RETRIES_MAX as u32 + 1
            );
        }
        RetransmitAction::Data(_) => return fail!("routed to the data retransmit path"),
    }
    assert_test!(
        !pcb_is_live(id),
        "the exhausted attempt left its PCB in the table"
    );
    pass!()
}

/// A completed handshake must take the SYN's timer with it. Left armed, it
/// fires against a `Data` PCB, and the wheel entry outlives the connection's
/// need for it.
///
/// The observable is the wheel's own pending count, taken across the scope that
/// owns the wheel: the token is moved out of the state during the transition,
/// so nothing else can be asked whether it was cancelled.
pub fn test_completed_handshake_retires_the_syn_timer() -> TestResult {
    let scope = match NetTestScope::enter_at_mock_ms(300) {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let _ = &scope;

    let before = crate::timer::wheel().pending_count();

    let (id, syn) = match tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT) {
        Ok(v) => v,
        Err(e) => return fail!("connect: {:?}", e),
    };
    tcp::arm_syn_retransmit(id);

    let armed = crate::timer::wheel().pending_count();
    assert_test!(
        armed > before,
        "connect armed no SYN timer ({} -> {}), so nothing below is exercised",
        before,
        armed
    );

    let syn_ack = super::tcp_common::make_header(
        REMOTE_PORT,
        syn.tuple.local_port,
        super::tcp_common::PEER_ISS,
        syn.seq_num.wrapping_add(1),
        TCP_FLAG_SYN | TCP_FLAG_ACK,
        32768,
    );
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &syn_ack, &[], &[], 0);
    assert_test!(
        pcb_is_live(id),
        "the synthetic SYN-ACK did not complete the handshake"
    );

    let after = crate::timer::wheel().pending_count();
    assert_eq_test!(
        after,
        before,
        "the established connection kept the SYN's retransmit timer armed"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_syn_is_retransmitted_with_the_original_iss,
    suite = tcp_syn_retransmit
);
slopos_testing::stest!(
    name = test_syn_retransmits_are_bounded_and_release_the_pcb,
    suite = tcp_syn_retransmit
);
slopos_testing::stest!(
    name = test_completed_handshake_retires_the_syn_timer,
    suite = tcp_syn_retransmit
);
