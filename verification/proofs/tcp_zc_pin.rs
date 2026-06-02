// TCP MSG_ZEROCOPY send-queue pin-lifetime proof.
//
// Machine-checks the two properties the kernel's TCP zero-copy send queue
// (`net/src/tcp/buffer.rs`'s `SendChunk::Zerocopy` + `poll_transmit`'s
// re-DMA-on-retransmit path) relies on — the obligation the TCP MSG_ZEROCOPY
// work names ("the pinned pages are read in-bounds on every (re)transmit and are
// never freed before the bytes are cumulatively ACKed"):
//
//   (INV-TCPZC-PIN-IN-BOUNDS) Every in-flight zero-copy segment's read window
//        `[pin_base, pin_base + len)` lies inside its pinned buffer
//        `[0, pin_len)` — so a transmit and every retransmit (re-DMA) of that
//        segment, and the cold-neighbor copy fallback, read only the segment's
//        own pinned bytes. The kernel builds the window from
//        `coalesce_io_runs(keepalive, base_off + intra, len)` /
//        `copy_out_frames(..)`, both bounded by `off + len <= pin.len`; modelled
//        here as: a segment only enters the queue with `pin_base + len <=
//        pin_len`, and no step ever widens an existing window.
//
//   (INV-TCPZC-HELD-UNTIL-ACK) A segment's pin is released only once its bytes
//        are cumulatively ACKed (`snd_una >= seq + len`) or the connection is
//        torn down. A Transmit (initial send or retransmit re-DMA) and a driver
//        TX Reclaim never drop a segment, so a retransmit always finds its pin
//        live; an Ack drops a head segment only when it is fully covered. This is
//        the no-use-after-free / no-free-mid-DMA guarantee for the retransmit
//        window (the refcounted `ZcNotifToken` enforces the matching
//        buffer-reusable signal at runtime; that weak-memory protocol is
//        audited-only — see verification/STATUS.md).

use vstd::prelude::*;

verus! {

// ===========================================================================
// Abstract send-queue state.
// ===========================================================================

/// One in-flight zero-copy segment: its sequence range `[seq, seq + len)` and
/// the pinned-page read window `[pin_base, pin_base + len)` within a pin of
/// `pin_len` bytes (mirrors `SendChunk::Zerocopy { base_off, len, .. }` /
/// `ZcSource { byte_start, len, .. }` in net/src/tcp/buffer.rs).
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

// ===========================================================================
// Transitions.
// ===========================================================================

pub enum SendStep {
    /// Enqueue a new zero-copy segment (`tcp::enqueue_zerocopy`).
    Send { seq: nat, len: nat, pin_base: nat, pin_len: nat },
    /// (Re)transmit segment `idx` — DMA / re-DMA from its pin. No state change:
    /// the read window and the queue are untouched, so a retransmit re-reads the
    /// same live pin (`poll_transmit` -> `segment_source` -> the leaf).
    Transmit { idx: nat },
    /// Driver reclaims one in-flight TX descriptor of segment `idx`. No queue
    /// change (the pin is held until cumulative ACK; only the refcount moves).
    Reclaim { idx: nat },
    /// Cumulative ACK up to `up_to`: advance `snd_una` and GC the head segment
    /// if it is now fully covered (`process_ack`'s head-first chunk drop).
    Ack { up_to: nat },
    /// Connection reset / close: drop every in-flight segment.
    Teardown,
}

/// `max(a, b)`.
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
            // Only a window that fits its pin ever enters the queue — exactly the
            // `off + len <= pin.len` bound `io_runs_at` / `copy_out_frames`
            // enforce at the source. An ill-formed send is a no-op (rejected).
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
                // Every old segment is unchanged; the appended one is in-bounds
                // by the guard. Discharge the quantifier per index.
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
            // Dropping the head (or nothing) only removes segments; the survivors
            // keep their (in-bounds) windows.
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

// ===========================================================================
// Whole-trace induction (mirrors vmcursor.rs / ring_bufpool.rs).
// ===========================================================================

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

/// Every reachable send-queue state keeps the invariant — the inductive
/// whole-execution guarantee.
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

// ===========================================================================
// Headline corollaries.
// ===========================================================================

/// (INV-TCPZC-PIN-IN-BOUNDS) In every reachable state, every in-flight
/// zero-copy segment's read window is inside its pin — so every transmit,
/// retransmit re-DMA, and copy fallback reads only the segment's own pinned
/// bytes (never out of bounds).
pub proof fn tcp_zc_pin_in_bounds(q0: SendQ, trace: Seq<SendStep>, i: int)
    requires
        sendq_init(q0),
        0 <= i < sendq_run(q0, trace).segs.len(),
    ensures
        sendq_run(q0, trace).segs[i].pin_base + sendq_run(q0, trace).segs[i].len
            <= sendq_run(q0, trace).segs[i].pin_len,
{
    sendq_inv_on_every_trace(q0, trace);
    // Instantiate the invariant's `forall` at this index.
    assert(seg_in_bounds(sendq_run(q0, trace).segs[i]));
}

/// (INV-TCPZC-HELD-UNTIL-ACK, retransmit half) A (re)transmit never drops a
/// segment from the queue: the pin a retransmit re-reads is still live and
/// unchanged. Reclaim is the same (the refcount moves; the queue does not).
pub proof fn transmit_keeps_pins(q: SendQ, idx: nat)
    ensures
        sendq_step(q, SendStep::Transmit { idx }).segs == q.segs,
        sendq_step(q, SendStep::Reclaim { idx }).segs == q.segs,
{
}

/// (INV-TCPZC-HELD-UNTIL-ACK, free half) An ACK drops the head segment only when
/// it is fully cumulatively covered (`seq + len <= snd_una'`); an un-acked
/// segment is never freed by ACK. So a pin is released only after its bytes are
/// ACKed (or by teardown) — no free before ACK / mid-retransmit-window.
pub proof fn ack_frees_only_covered(q: SendQ, up_to: nat)
    requires
        q.segs.len() > 0,
        // The head is NOT yet cumulatively covered after this ACK...
        q.segs[0].seq + q.segs[0].len > max_nat(q.snd_una, up_to),
    ensures
        // ...so the ACK does not drop it: the pin stays live.
        sendq_step(q, SendStep::Ack { up_to }).segs == q.segs,
{
}

/// Load-bearing non-vacuity witness (the `ring_bufpool.rs` idiom): a queue that
/// admits a segment whose read window runs past its pin violates the invariant —
/// i.e. INV-TCPZC-PIN-IN-BOUNDS genuinely forbids an out-of-bounds DMA read, it
/// is not vacuously true. (`sendq_step`'s `Send` guard is exactly what stops such
/// a segment from ever entering a reachable queue.)
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
