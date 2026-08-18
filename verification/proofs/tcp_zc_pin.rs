// Verus mirror of the kernel's TCP MSG_ZEROCOPY send queue
// (`net/src/tcp/buffer.rs`'s `SendChunk::Zerocopy` + `poll_transmit`'s
// re-DMA-on-retransmit path):
//
//   (INV-TCPZC-PIN-IN-BOUNDS) Every in-flight zero-copy segment's read window
//        `[pin_base, pin_base + len)` lies inside its pinned buffer
//        `[0, pin_len)`, so a transmit, every retransmit re-DMA and the
//        cold-neighbor copy fallback read only that segment's own pinned bytes.
//
//   (INV-TCPZC-HELD-UNTIL-ACK) A segment's pin is released only once its bytes
//        are cumulatively ACKed (`snd_una >= seq + len`) or the connection is
//        torn down; Transmit and driver TX Reclaim never drop a segment. The
//        refcounted `ZcNotifToken` carrying the matching buffer-reusable signal
//        at runtime is a weak-memory protocol Verus cannot model — it stays
//        audited-only, see verification/STATUS.md.

use vstd::prelude::*;

verus! {

/// One in-flight zero-copy segment: its sequence range `[seq, seq + len)` and
/// the pinned-page read window within a pin of `pin_len` bytes. Mirrors
/// `SendChunk::Zerocopy` / `ZcSource` in net/src/tcp/buffer.rs.
pub struct SegState {
    pub seq: nat,
    pub len: nat,
    pub pin_base: nat,
    pub pin_len: nat,
}

/// The send queue: the in-flight zero-copy segments in stream order, and the
/// cumulative-ACK boundary `snd_una`.
pub struct SendQ {
    pub segs: Seq<SegState>,
    pub snd_una: nat,
}

/// A segment's read window is inside its pin (INV-TCPZC-PIN-IN-BOUNDS).
pub open spec fn seg_in_bounds(s: SegState) -> bool {
    s.pin_base + s.len <= s.pin_len
}

pub open spec fn sendq_inv(q: SendQ) -> bool {
    forall|i: int| 0 <= i < q.segs.len() ==> #[trigger] seg_in_bounds(q.segs[i])
}

pub enum SendStep {
    /// Enqueue a new zero-copy segment (`tcp::enqueue_zerocopy`).
    Send { seq: nat, len: nat, pin_base: nat, pin_len: nat },
    /// (Re)transmit segment `idx` — DMA / re-DMA from its pin (`poll_transmit`
    /// -> `segment_source`). No state change, so a retransmit re-reads the same
    /// live pin.
    Transmit { idx: nat },
    /// Driver reclaims one in-flight TX descriptor of segment `idx`. Only the
    /// refcount moves; the pin is held until cumulative ACK.
    Reclaim { idx: nat },
    /// Cumulative ACK up to `up_to`: advance `snd_una` and GC the head segment
    /// if it is now fully covered (`process_ack`'s head-first chunk drop).
    Ack { up_to: nat },
    /// Connection reset / close: drop every in-flight segment.
    Teardown,
}

pub open spec fn max_nat(a: nat, b: nat) -> nat {
    if a >= b {
        a
    } else {
        b
    }
}

pub open spec fn sendq_step(q: SendQ, t: SendStep) -> SendQ {
    match t {
        SendStep::Send { seq, len, pin_base, pin_len } => {
            // Mirrors the `off + len <= pin.len` bound `io_runs_at` /
            // `copy_out_frames` enforce at the source.
            if pin_base + len <= pin_len {
                SendQ {
                    segs: q.segs.push(SegState { seq, len, pin_base, pin_len }),
                    snd_una: q.snd_una,
                }
            } else {
                q
            }
        },
        SendStep::Transmit { idx: _ } => q,
        SendStep::Reclaim { idx: _ } => q,
        SendStep::Ack { up_to } => {
            let una = max_nat(q.snd_una, up_to);
            if q.segs.len() > 0 && q.segs[0].seq + q.segs[0].len <= una {
                SendQ { segs: q.segs.drop_first(), snd_una: una }
            } else {
                SendQ { segs: q.segs, snd_una: una }
            }
        },
        SendStep::Teardown => SendQ { segs: Seq::empty(), snd_una: q.snd_una },
    }
}

pub proof fn sendq_step_preserves(q: SendQ, t: SendStep)
    requires
        sendq_inv(q),
    ensures
        sendq_inv(sendq_step(q, t)),
{
    let q2 = sendq_step(q, t);
    match t {
        SendStep::Send { seq, len, pin_base, pin_len } => {
            if pin_base + len <= pin_len {
                assert(forall|i: int| 0 <= i < q2.segs.len() ==> #[trigger]
                    seg_in_bounds(q2.segs[i])) by {
                    assert(forall|i: int| 0 <= i < q.segs.len() ==> q2.segs[i] == q.segs[i]);
                    assert(q2.segs[q.segs.len() as int] == SegState { seq, len, pin_base, pin_len });
                }
            }
        },
        SendStep::Transmit { idx: _ } => {},
        SendStep::Reclaim { idx: _ } => {},
        SendStep::Ack { up_to } => {
            assert(forall|i: int| 0 <= i < q2.segs.len() ==> #[trigger]
                seg_in_bounds(q2.segs[i])) by {
                if q.segs.len() > 0 && q.segs[0].seq + q.segs[0].len <= max_nat(q.snd_una, up_to) {
                    assert(q2.segs =~= q.segs.drop_first());
                    assert(forall|i: int| 0 <= i < q2.segs.len() ==> q2.segs[i] == q.segs[i + 1]);
                }
            }
        },
        SendStep::Teardown => {},
    }
}

pub open spec fn sendq_init(q: SendQ) -> bool {
    q.segs.len() == 0
}

pub proof fn sendq_init_inv(q: SendQ)
    requires
        sendq_init(q),
    ensures
        sendq_inv(q),
{
}

pub open spec fn sendq_run(q: SendQ, trace: Seq<SendStep>) -> SendQ
    decreases trace.len(),
{
    if trace.len() == 0 {
        q
    } else {
        sendq_step(sendq_run(q, trace.drop_last()), trace.last())
    }
}

/// Every reachable send-queue state keeps the invariant.
pub proof fn sendq_inv_on_every_trace(q0: SendQ, trace: Seq<SendStep>)
    requires
        sendq_init(q0),
    ensures
        sendq_inv(sendq_run(q0, trace)),
    decreases trace.len(),
{
    if trace.len() == 0 {
        sendq_init_inv(q0);
    } else {
        sendq_inv_on_every_trace(q0, trace.drop_last());
        sendq_step_preserves(sendq_run(q0, trace.drop_last()), trace.last());
    }
}

/// (INV-TCPZC-PIN-IN-BOUNDS) In every reachable state, every in-flight
/// zero-copy segment's read window is inside its pin.
pub proof fn tcp_zc_pin_in_bounds(q0: SendQ, trace: Seq<SendStep>, i: int)
    requires
        sendq_init(q0),
        0 <= i < sendq_run(q0, trace).segs.len(),
    ensures
        sendq_run(q0, trace).segs[i].pin_base + sendq_run(q0, trace).segs[i].len
            <= sendq_run(q0, trace).segs[i].pin_len,
{
    sendq_inv_on_every_trace(q0, trace);
    assert(seg_in_bounds(sendq_run(q0, trace).segs[i]));
}

/// (INV-TCPZC-HELD-UNTIL-ACK, retransmit half) Neither a (re)transmit nor a
/// driver reclaim drops a segment, so the pin a retransmit re-reads is live.
pub proof fn transmit_keeps_pins(q: SendQ, idx: nat)
    ensures
        sendq_step(q, SendStep::Transmit { idx }).segs == q.segs,
        sendq_step(q, SendStep::Reclaim { idx }).segs == q.segs,
{
}

/// (INV-TCPZC-HELD-UNTIL-ACK, free half) An ACK drops the head segment only
/// when it is fully cumulatively covered, so a pin is released only after its
/// bytes are ACKed or by teardown — never mid-retransmit-window.
pub proof fn ack_frees_only_covered(q: SendQ, up_to: nat)
    requires
        q.segs.len() > 0,
        q.segs[0].seq + q.segs[0].len > max_nat(q.snd_una, up_to),
    ensures
        sendq_step(q, SendStep::Ack { up_to }).segs == q.segs,
{
}

/// Non-vacuity witness: a segment whose read window runs past its pin violates
/// the invariant, so INV-TCPZC-PIN-IN-BOUNDS genuinely forbids an out-of-bounds
/// DMA read. `sendq_step`'s `Send` guard is what keeps such a segment out.
pub proof fn witness_oob_segment_breaks_inv()
    ensures
        !sendq_inv(SendQ {
            segs: seq![SegState { seq: 0, len: 8, pin_base: 4096, pin_len: 4096 }],
            snd_una: 0,
        }),
{
    let bad = SegState { seq: 0, len: 8, pin_base: 4096, pin_len: 4096 };
    assert(!seg_in_bounds(bad));
    assert(SendQ {
        segs: seq![bad],
        snd_una: 0,
    }.segs[0] == bad);
}

} // verus!
