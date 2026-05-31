//! Async time: `sleep` and `timeout`, built on `OP_TIMEOUT`.

use core::future::Future;

use super::{Either2, select2, timeout as timeout_op};

const NS_PER_MS: u64 = 1_000_000;

/// Sleep for `ms` milliseconds (an `OP_TIMEOUT` that resolves with `-ETIME`).
pub async fn sleep_ms(ms: u64) {
    let _ = timeout_op(ms.saturating_mul(NS_PER_MS)).await;
}

/// Returned by [`timeout`] when the deadline elapses before `fut` resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Elapsed;

/// Run `fut` with a deadline of `ms` milliseconds. Resolves to `Ok(output)`
/// if `fut` completes first, or `Err(Elapsed)` if the timer fires first (the
/// losing future is dropped, cancelling its in-flight op).
pub async fn timeout<F: Future>(ms: u64, fut: F) -> Result<F::Output, Elapsed> {
    // Box::pin makes an arbitrary (possibly !Unpin) future usable by the
    // by-reference `select2`.
    let fut = Box::pin(fut);
    let timer = timeout_op(ms.saturating_mul(NS_PER_MS));
    match select2(fut, timer).await {
        Either2::A(v) => Ok(v),
        Either2::B(_) => Err(Elapsed),
    }
}
