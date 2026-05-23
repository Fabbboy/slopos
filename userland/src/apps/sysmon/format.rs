use std::format;
use std::string::String;

use slopos_abi::draw::Color32;
use slopos_abi::task::TaskPriority;

use crate::syscall::UserTaskEntry;

use super::{COLOR_DIM, COLOR_STATE_BLOCK, COLOR_STATE_READY, COLOR_STATE_RUN};

pub(crate) fn task_name_bytes(task: &UserTaskEntry) -> &[u8] {
    let end = task
        .name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(task.name.len());
    &task.name[..end]
}

pub(crate) fn task_name_string(task: &UserTaskEntry) -> String {
    task_name_from_slice(task_name_bytes(task))
}

fn task_name_from_slice(bytes: &[u8]) -> String {
    if let core::result::Result::Ok(s) = core::str::from_utf8(bytes) {
        return s.to_string();
    }

    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_graphic() || b == b' ' {
            out.push(b as char);
        } else {
            out.push('?');
        }
    }
    out
}

pub(crate) fn task_state(state: u8) -> (&'static str, Color32) {
    match state {
        2 => ("Run", COLOR_STATE_RUN),
        3 => ("Block", COLOR_STATE_BLOCK),
        1 => ("Ready", COLOR_STATE_READY),
        5 => ("WillBlk", COLOR_STATE_BLOCK),
        4 => ("Dead", COLOR_DIM),
        _ => ("--", COLOR_DIM),
    }
}

pub(crate) fn priority_label(priority: u8) -> &'static str {
    match TaskPriority::from_u8(priority) {
        TaskPriority::High => "Hi",
        TaskPriority::KernelIo => "KIO",
        TaskPriority::Normal => "Norm",
        TaskPriority::Low => "Low",
        TaskPriority::Idle => "Idle",
    }
}

/// True for the per-CPU kernel idle tasks. Sysmon dims these rows so they
/// don't dominate the visible process list. The syscall boundary now
/// rejects userland spawn calls with `Idle` priority, so this check is
/// sufficient — only the kernel idle loop can carry that priority.
pub(crate) fn is_idle_task(task: &UserTaskEntry) -> bool {
    TaskPriority::from_u8(task.priority) == TaskPriority::Idle
}

pub(crate) fn trim_ascii(bytes: &[u8]) -> String {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == 0 || bytes[end - 1] == b' ') {
        end -= 1;
    }
    let slice = &bytes[..end];
    if let core::result::Result::Ok(s) = core::str::from_utf8(slice) {
        return s.to_string();
    }

    let mut out = String::with_capacity(slice.len());
    for &b in slice {
        if b.is_ascii_graphic() || b == b' ' {
            out.push(b as char);
        } else {
            out.push('?');
        }
    }
    out
}

pub(crate) fn truncate_name(name: &str, max_chars: usize) -> String {
    let total = name.chars().count();
    if total <= max_chars {
        return name.to_string();
    }

    let mut out = String::new();
    for ch in name.chars().take(max_chars.saturating_sub(1)) {
        out.push(ch);
    }
    out.push('~');
    out
}

pub(crate) fn format_bytes_mib(bytes: u64) -> String {
    let whole = bytes / (1024 * 1024);
    let frac = ((bytes % (1024 * 1024)).saturating_mul(10)) / (1024 * 1024);
    if frac == 0 {
        format!("{} MiB", whole)
    } else {
        format!("{}.{} MiB", whole, frac)
    }
}

pub(crate) fn format_uptime(ms: u64) -> String {
    let total_sec = ms / 1000;
    let days = total_sec / 86_400;
    let rem = total_sec % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    format!("{}d {:02}:{:02}:{:02}", days, h, m, s)
}

pub(crate) fn format_runtime(us: u64) -> String {
    let total_cs = us / 10_000;
    let cs = total_cs % 100;
    let total_sec = total_cs / 100;
    let h = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = total_sec % 60;
    if h == 0 {
        format!("{:02}:{:02}.{:02}", m, s, cs)
    } else {
        format!("{}:{:02}:{:02}.{:02}", h, m, s, cs)
    }
}

pub(crate) fn format_pct(pct_x10: u32) -> String {
    format!("{}.{}%", pct_x10 / 10, pct_x10 % 10)
}

pub(crate) fn format_number(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + ((len.saturating_sub(1)) / 3));
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub(crate) fn format_cpu_features(features: u64) -> String {
    let ecx = features as u32;
    let edx = (features >> 32) as u32;
    let mut parts: String = String::new();
    let mut push_part = |part: &str| {
        if !parts.is_empty() {
            parts.push(' ');
        }
        parts.push_str(part);
    };
    if edx & (1 << 25) != 0 {
        push_part("SSE");
    }
    if edx & (1 << 26) != 0 {
        push_part("SSE2");
    }
    if ecx & (1 << 0) != 0 {
        push_part("SSE3");
    }
    if ecx & (1 << 9) != 0 {
        push_part("SSSE3");
    }
    if ecx & (1 << 19) != 0 {
        push_part("SSE4.1");
    }
    if ecx & (1 << 20) != 0 {
        push_part("SSE4.2");
    }
    if ecx & (1 << 28) != 0 {
        push_part("AVX");
    }
    if ecx & (1 << 26) != 0 {
        push_part("XSAVE");
    }
    if ecx & (1 << 21) != 0 {
        push_part("x2APIC");
    }
    if edx & (1 << 9) != 0 {
        push_part("APIC");
    }
    if edx & (1 << 4) != 0 {
        push_part("TSC");
    }
    if parts.is_empty() {
        return format!("0x{:016x}", features);
    }
    parts
}
