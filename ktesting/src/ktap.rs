//! KTAP-grammar emitter.
//!
//! Every line carries the literal `KTAP\t` prefix so a host parser can ignore
//! interleaved klog; diagnostic YAML blocks add two spaces after the tab.

use slopos_ostd::klog_info;

use crate::registry::TestDesc;
use crate::result::TestResult;

/// Per-failing-test cap on captured-log emission, to bound serial output.
const MAX_LOG_EMIT: usize = 4096;

/// Stands in for `time_ms=N` when the harness had no monotonic time base to
/// measure with. Occupies the field's position so a trailing directive keeps
/// the leading space the host parser matches on.
const NO_TIME_BASE: &str = "NO_TIME_BASE";

pub fn emit_header(plan: u32) {
    klog_info!("KTAP\tTAP version 14");
    klog_info!("KTAP\t1..{}", plan);
}

pub fn emit_ok(idx: u32, desc: &TestDesc, time_ms: Option<u32>, suffix: Option<&str>) {
    match (time_ms, suffix) {
        (Some(ms), Some(s)) => klog_info!(
            "KTAP\tok {} - {}::{} # time_ms={} {}",
            idx,
            desc.module,
            desc.name,
            ms,
            s,
        ),
        (Some(ms), None) => klog_info!(
            "KTAP\tok {} - {}::{} # time_ms={}",
            idx,
            desc.module,
            desc.name,
            ms,
        ),
        (None, Some(s)) => klog_info!(
            "KTAP\tok {} - {}::{} # {} {}",
            idx,
            desc.module,
            desc.name,
            NO_TIME_BASE,
            s,
        ),
        (None, None) => klog_info!(
            "KTAP\tok {} - {}::{} # {}",
            idx,
            desc.module,
            desc.name,
            NO_TIME_BASE,
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
    time_ms: Option<u32>,
    outcome: TestResult,
    log: &[u8],
    truncated_bytes: usize,
) {
    match time_ms {
        Some(ms) => klog_info!(
            "KTAP\tnot ok {} - {}::{} # time_ms={}",
            idx,
            desc.module,
            desc.name,
            ms,
        ),
        None => klog_info!(
            "KTAP\tnot ok {} - {}::{} # {}",
            idx,
            desc.module,
            desc.name,
            NO_TIME_BASE,
        ),
    }
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

// Subtest lines reach the wire *before* their parent's `ok`/`not ok` line; the
// two-space indent is what keys the host parser into nested mode.

/// Pass subtest line. `sub_idx` is the 1-based position within the parent.
pub fn emit_subtest_ok(sub_idx: u32, name: &str) {
    klog_info!("KTAP\t  ok {} - {}", sub_idx, name);
}

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
