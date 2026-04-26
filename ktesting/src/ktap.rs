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
