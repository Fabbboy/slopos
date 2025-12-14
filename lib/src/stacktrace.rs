use core::ffi::c_int;
use crate::cpu;
use crate::klog::{self, KlogLevel};

const STACKTRACE_MAX_LOCAL: usize = 32;

#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Copy, Clone)]
pub struct stacktrace_entry {
    pub frame_pointer: u64,
    pub return_address: u64,
}

#[inline(always)]
fn read_frame_pointer() -> u64 {
    cpu::read_rbp()
}

fn is_canonical_address(address: u64) -> bool {
    let upper = address >> 47;
    upper == 0 || upper == 0x1FFFF
}

fn basic_sanity_check(current_rbp: u64, next_rbp: u64) -> bool {
    if next_rbp <= current_rbp {
        return false;
    }
    if next_rbp - current_rbp > (1u64 << 20) {
        return false;
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn stacktrace_capture_from(
    mut rbp: u64,
    entries: *mut stacktrace_entry,
    max_entries: c_int,
) -> c_int {
    if entries.is_null() || max_entries <= 0 {
        return 0;
    }

    let mut count = 0;
    let max_entries = max_entries as usize;

    while rbp != 0 && count < max_entries {
        if rbp & 0x7 != 0 || !is_canonical_address(rbp) {
            break;
        }

        unsafe {
            let frame = rbp as *const u64;
            let next_rbp = *frame;
            let return_address = *frame.add(1);

            let entry_ptr = entries.add(count);
            (*entry_ptr).frame_pointer = rbp;
            (*entry_ptr).return_address = return_address;
            count += 1;

            if !is_canonical_address(next_rbp) {
                break;
            }
            if !basic_sanity_check(rbp, next_rbp) {
                break;
            }

            rbp = next_rbp;
        }
    }

    count as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn stacktrace_capture(
    entries: *mut stacktrace_entry,
    max_entries: c_int,
) -> c_int {
    let rbp = read_frame_pointer();
    stacktrace_capture_from(rbp, entries, max_entries)
}

#[unsafe(no_mangle)]
pub extern "C" fn stacktrace_dump_from(rbp: u64, max_frames: c_int) {
    if max_frames <= 0 {
        return;
    }

    let max = max_frames.clamp(0, STACKTRACE_MAX_LOCAL as c_int) as usize;
    let mut local_entries: [stacktrace_entry; STACKTRACE_MAX_LOCAL] =
        [stacktrace_entry { frame_pointer: 0, return_address: 0 }; STACKTRACE_MAX_LOCAL];

    let captured = stacktrace_capture_from(rbp, local_entries.as_mut_ptr(), max as c_int);
    if captured <= 0 {
        klog::log_line(KlogLevel::Info, "STACKTRACE: <empty>");
        return;
    }

    klog::log_line(KlogLevel::Info, "STACKTRACE:");
    for i in 0..captured as usize {
        let entry = &local_entries[i];
        crate::klog_info!(
            "  #{} rbp=0x{:x} rip=0x{:x}",
            i,
            entry.frame_pointer,
            entry.return_address
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn stacktrace_dump(max_frames: c_int) {
    let rbp = read_frame_pointer();
    stacktrace_dump_from(rbp, max_frames);
}

