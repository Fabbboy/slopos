//! KTAP-grammar emitter.
//!
//! Each emitted line is prefixed with the literal `KTAP\t` (an ASCII tab),
//! so a host parser can tolerate kernel klog interleaving by simply
//! ignoring lines that don't start with the prefix. Diagnostic YAML blocks
//! are indented `KTAP\t  ` (two spaces after the tab).

use slopos_utils::klog_info;

use crate::registry::TestDesc;
use crate::result::TestResult;

/// Truncate captured-log emission at this many bytes per failing test to
/// keep serial output bounded; the host can re-run the test in `--raw`
/// mode if it needs the full transcript.
const MAX_LOG_EMIT: usize = 4096;

pub fn emit_header(plan: u32) {
    klog_info!("KTAP\tTAP version 14");
    klog_info!("KTAP\t1..{}", plan);
}

pub fn emit_ok(idx: u32, desc: &TestDesc, time_ms: u32, suffix: Option<&str>) {
    match suffix {
        Some(s) => klog_info!(
            "KTAP\tok {} - {}::{} # time_ms={} {}",
            idx,
            desc.module,
            desc.name,
            time_ms,
            s,
        ),
        None => klog_info!(
            "KTAP\tok {} - {}::{} # time_ms={}",
            idx,
            desc.module,
            desc.name,
            time_ms,
        ),
    }
}

pub fn emit_skip(idx: u32, desc: &TestDesc, reason: &str) {
    klog_info!(
        "KTAP\tok {} - {}::{} # SKIP {}",
        idx,
        desc.module,
        desc.name,
        reason
    );
}

pub fn emit_not_ok(
    idx: u32,
    desc: &TestDesc,
    time_ms: u32,
    outcome: TestResult,
    log: &[u8],
    truncated_bytes: usize,
) {
    klog_info!(
        "KTAP\tnot ok {} - {}::{} # time_ms={}",
        idx,
        desc.module,
        desc.name,
        time_ms,
    );
    klog_info!("KTAP\t  ---");
    klog_info!("KTAP\t  outcome: {:?}", outcome);
    klog_info!("KTAP\t  file: {}:{}", desc.file, desc.line);
    klog_info!("KTAP\t  log: |");

    let emit_slice = if log.len() > MAX_LOG_EMIT {
        &log[log.len() - MAX_LOG_EMIT..]
    } else {
        log
    };
    let head_skipped = log.len().saturating_sub(emit_slice.len());
    if head_skipped > 0 {
        klog_info!("KTAP\t   [head trimmed: {} bytes]", head_skipped);
    }
    for line in emit_slice.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let s = core::str::from_utf8(line).unwrap_or("<non-utf8 log line>");
        klog_info!("KTAP\t   {}", s);
    }
    if truncated_bytes > 0 {
        klog_info!(
            "KTAP\t   [tail trimmed: {} bytes lost to ring overflow]",
            truncated_bytes
        );
    }
    klog_info!("KTAP\t  ...");
}

pub fn emit_footer(elapsed_ms: u32, pass: u32, fail: u32, skip: u32, over_time: u32) {
    klog_info!(
        "KTAP\t# elapsed_ms={} pass={} fail={} skip={} over_time={}",
        elapsed_ms,
        pass,
        fail,
        skip,
        over_time
    );
}

pub fn emit_bail(reason: &str) {
    klog_info!("KTAP\tBail out! {}", reason);
}

// =============================================================================
// Nested subtest emit (Phase 3 utests)
// =============================================================================
//
// Subtests are emitted *during* the parent utest's run, while the parent
// `ok N - …`/`not ok N - …` line follows after `(desc.run)()` returns.
// Result on the wire: subtest lines come *before* their parent line. The
// host wrapper (Phase 4) treats subtests as siblings of the next-emitted
// parent line. Two-space indent before `ok`/`not ok` keys the parser into
// nested mode.

/// Pass subtest line. `sub_idx` is the 1-based position within the parent.
pub fn emit_subtest_ok(sub_idx: u32, name: &str) {
    klog_info!("KTAP\t  ok {} - {}", sub_idx, name);
}

/// Fail subtest line. `msg` is the optional diagnostic; empty strings are
/// emitted without a `# …` suffix to keep the wire-format clean.
pub fn emit_subtest_not_ok(sub_idx: u32, name: &str, msg: &str) {
    if msg.is_empty() {
        klog_info!("KTAP\t  not ok {} - {}", sub_idx, name);
    } else {
        klog_info!("KTAP\t  not ok {} - {} # {}", sub_idx, name, msg);
    }
}

/// Skip subtest line. KTAP encodes skips as `ok` with a `# SKIP` suffix.
pub fn emit_subtest_skip(sub_idx: u32, name: &str) {
    klog_info!("KTAP\t  ok {} - {} # SKIP", sub_idx, name);
}
