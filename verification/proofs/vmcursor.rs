// Verus mirror of the two in-bounds properties the kernel's `VmReader` /
// `VmWriter` (`slopos-ostd/src/mm/vmcursor.rs`) rely on:
//
//   (INV-VMCURSOR-NO-CROSS-FRAME) A per-frame volatile copy moves at most
//        `page - intra_off` bytes, so it stays inside the current frame and
//        `UFrame::copy_{out,in}_volatile`'s `offset + len <= 4096` check
//        always passes — the machine-checked analogue of the compile-fail
//        "no `&[u8]` over a UFrame" discipline (AD-3).
//
//   (INV-VMCURSOR-FRAME-IN-RANGE) Whenever a copy occurs (`remaining > 0`) the
//        cursor points at a frame that exists, so `frames[frame_idx]` is always
//        a valid index. Preserved across every advance by the byte-accounting
//        invariant `frame_idx*page + intra_off + remaining <= n_frames*page`.

use vstd::prelude::*;

verus! {

/// Frame size in bytes.
pub open spec fn page() -> nat {
    4096
}

pub open spec fn min3(a: nat, b: nat, c: nat) -> nat {
    let m = if a <= b { a } else { b };
    if m <= c { m } else { c }
}

/// Bytes a single per-frame volatile copy moves. Mirrors the
/// `self.remaining.min(page_left).min(dst.len())` clamp in `read`/`write`.
pub open spec fn cursor_chunk(remaining: nat, page_left: nat, buf_len: nat) -> nat {
    min3(remaining, page_left, buf_len)
}

/// (INV-VMCURSOR-NO-CROSS-FRAME) A copy never exceeds the bytes left in the
/// current frame.
pub proof fn chunk_within_frame(remaining: nat, page_left: nat, buf_len: nat)
    ensures
        cursor_chunk(remaining, page_left, buf_len) <= page_left,
{
}

/// A copy never consumes more than the logical bytes remaining (so
/// `remaining - chunk` never underflows).
pub proof fn chunk_within_remaining(remaining: nat, page_left: nat, buf_len: nat)
    ensures
        cursor_chunk(remaining, page_left, buf_len) <= remaining,
{
}

pub struct CursorState {
    /// Index of the frame the cursor points into.
    pub frame_idx: nat,
    /// Byte offset within `frames[frame_idx]`.
    pub intra_off: nat,
    /// Bytes still available in the logical range.
    pub remaining: nat,
    /// Number of frames in the pinned chain.
    pub n_frames: nat,
}

pub open spec fn cursor_inv(s: CursorState) -> bool {
    &&& s.intra_off < page()
    &&& s.frame_idx * page() + s.intra_off + s.remaining <= s.n_frames * page()
}

/// `a*p < b*p` with `p > 0` cancels to `a < b`.
pub proof fn mul_lt_cancel(a: nat, b: nat, p: nat)
    requires
        p > 0,
        a * p < b * p,
    ensures
        a < b,
{
    assert(a < b) by (nonlinear_arith)
        requires
            p > 0,
            a * p < b * p,
    ;
}

/// (INV-VMCURSOR-FRAME-IN-RANGE) A reachable copy targets a valid frame.
pub proof fn cursor_frame_in_range(s: CursorState)
    requires
        cursor_inv(s),
        s.remaining > 0,
    ensures
        s.frame_idx < s.n_frames,
{
    assert(s.frame_idx * page() < s.n_frames * page());
    mul_lt_cancel(s.frame_idx, s.n_frames, page());
}

/// One per-frame copy of `want`-bounded bytes: advance `intra_off` by the
/// clamped chunk, rolling over to the next frame when it fills the page.
pub open spec fn cursor_step(s: CursorState, want: nat) -> CursorState {
    let page_left = (page() - s.intra_off) as nat;
    let c = cursor_chunk(s.remaining, page_left, want);
    let new_off = (s.intra_off + c) as nat;
    if new_off == page() {
        CursorState {
            frame_idx: (s.frame_idx + 1) as nat,
            intra_off: 0,
            remaining: (s.remaining - c) as nat,
            n_frames: s.n_frames,
        }
    } else {
        CursorState {
            frame_idx: s.frame_idx,
            intra_off: new_off,
            remaining: (s.remaining - c) as nat,
            n_frames: s.n_frames,
        }
    }
}

pub proof fn cursor_step_preserves(s: CursorState, want: nat)
    requires
        cursor_inv(s),
    ensures
        cursor_inv(cursor_step(s, want)),
{
    let page_left = (page() - s.intra_off) as nat;
    chunk_within_frame(s.remaining, page_left, want);
    chunk_within_remaining(s.remaining, page_left, want);
    let c = cursor_chunk(s.remaining, page_left, want);
    // c <= page_left = page - intra_off  ==>  intra_off + c <= page.
    if (s.intra_off + c) as nat == page() {
        // Roll to the next frame; (frame_idx+1)*page distributes.
        assert((s.frame_idx + 1) * page() == s.frame_idx * page() + page()) by (nonlinear_arith);
        // (frame_idx+1)*page + (remaining - c)
        //   = frame_idx*page + page + remaining - c
        //   = frame_idx*page + intra_off + remaining   [c = page - intra_off]
        //   <= n_frames*page                            [inv]
    } else {
        // Stay in the frame; new_off = intra_off + c <= page, and != page so < page.
        // frame_idx*page + (intra_off + c) + (remaining - c)
        //   = frame_idx*page + intra_off + remaining <= n_frames*page  [inv]
    }
}

/// Canonical initial cursor: starts at frame 0 with an in-page base offset (the
/// `PinnedUserBuffer` `base_off`), spanning `remaining` bytes that fit in the
/// chain. (`VmReader::new` rejects any range that would run past the chain, so
/// every constructed cursor satisfies this.)
pub open spec fn cursor_init(s: CursorState) -> bool {
    &&& s.frame_idx == 0
    &&& s.intra_off < page()
    &&& s.intra_off + s.remaining <= s.n_frames * page()
}

pub proof fn cursor_init_inv(s: CursorState)
    requires
        cursor_init(s),
    ensures
        cursor_inv(s),
{
    assert(s.frame_idx * page() == 0);
}

pub open spec fn cursor_run(s: CursorState, trace: Seq<nat>) -> CursorState
    decreases trace.len(),
{
    if trace.len() == 0 {
        s
    } else {
        cursor_step(cursor_run(s, trace.drop_last()), trace.last())
    }
}

/// Every reachable cursor state satisfies the invariant — the inductive
/// whole-execution guarantee (mirrors ring_bufpool.rs's main theorem).
pub proof fn cursor_inv_on_every_trace(s0: CursorState, trace: Seq<nat>)
    requires
        cursor_init(s0),
    ensures
        cursor_inv(cursor_run(s0, trace)),
    decreases trace.len(),
{
    if trace.len() == 0 {
        cursor_init_inv(s0);
    } else {
        cursor_inv_on_every_trace(s0, trace.drop_last());
        cursor_step_preserves(cursor_run(s0, trace.drop_last()), trace.last());
    }
}

/// The headline corollary: in every reachable state the next per-frame copy
/// stays inside the current frame (`intra_off + chunk <= page` — no cross-frame
/// slice), and whenever a copy happens the cursor points at a valid frame
/// (`frame_idx < n_frames`).
pub proof fn cursor_read_in_bounds(s0: CursorState, trace: Seq<nat>, want: nat)
    requires
        cursor_init(s0),
    ensures
        cursor_run(s0, trace).intra_off
            + cursor_chunk(
                cursor_run(s0, trace).remaining,
                (page() - cursor_run(s0, trace).intra_off) as nat,
                want,
            ) <= page(),
        cursor_run(s0, trace).remaining > 0 ==> cursor_run(s0, trace).frame_idx
            < cursor_run(s0, trace).n_frames,
{
    let s = cursor_run(s0, trace);
    cursor_inv_on_every_trace(s0, trace);
    chunk_within_frame(s.remaining, (page() - s.intra_off) as nat, want);
    if s.remaining > 0 {
        cursor_frame_in_range(s);
    }
}

} // verus!
