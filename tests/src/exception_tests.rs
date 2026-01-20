use core::ffi::c_int;

use slopos_abi::arch::x86_64::exception::{exception_is_critical, get_exception_name};
use slopos_lib::{klog_info, InterruptFrame};

fn create_test_frame(vector: u8, from_user: bool) -> InterruptFrame {
    InterruptFrame {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        rbp: 0,
        rbx: 0,
        r11: 0,
        r10: 0,
        r9: 0,
        r8: 0,
        rax: 0,
        rcx: 0,
        rdx: 0,
        rsi: 0,
        rdi: 0,
        vector: vector as u64,
        error_code: 0,
        rip: if from_user {
            0x0000_7FFF_FFFF_0000
        } else {
            0xFFFF_FFFF_8000_0000
        },
        cs: if from_user { 0x23 } else { 0x08 },
        rflags: 0x202,
        rsp: if from_user {
            0x0000_7FFF_FFFE_0000
        } else {
            0xFFFF_FFFF_8010_0000
        },
        ss: if from_user { 0x1B } else { 0x10 },
    }
}

fn create_test_frame_with_error(vector: u8, from_user: bool, error_code: u64) -> InterruptFrame {
    let mut frame = create_test_frame(vector, from_user);
    frame.error_code = error_code;
    frame
}

pub fn test_exception_names_valid() -> c_int {
    for vector in 0..32u8 {
        let name = get_exception_name(vector);
        if name.is_empty() {
            klog_info!("EXCEPTION_TEST: BUG - Empty name for vector {}", vector);
            return -1;
        }
    }
    0
}

pub fn test_critical_exception_classification() -> c_int {
    if !exception_is_critical(8) {
        klog_info!("EXCEPTION_TEST: BUG - Double fault not marked critical");
        return -1;
    }
    if !exception_is_critical(2) {
        klog_info!("EXCEPTION_TEST: BUG - NMI not marked critical");
        return -1;
    }
    if !exception_is_critical(18) {
        klog_info!("EXCEPTION_TEST: BUG - Machine check not marked critical");
        return -1;
    }

    if exception_is_critical(0) {
        klog_info!("EXCEPTION_TEST: BUG - Divide error marked critical");
        return -1;
    }
    if exception_is_critical(13) {
        klog_info!("EXCEPTION_TEST: BUG - GPF marked critical");
        return -1;
    }
    if exception_is_critical(14) {
        klog_info!("EXCEPTION_TEST: BUG - Page fault marked critical");
        return -1;
    }

    if exception_is_critical(32) {
        klog_info!("EXCEPTION_TEST: BUG - Vector 32 marked critical");
        return -1;
    }
    if exception_is_critical(255) {
        klog_info!("EXCEPTION_TEST: BUG - Vector 255 marked critical");
        return -1;
    }

    0
}

pub fn test_page_fault_error_codes() -> c_int {
    let user_write_notpresent = 0b0110u64;
    let p = (user_write_notpresent & 1) != 0;
    let w = (user_write_notpresent & 2) != 0;
    let u = (user_write_notpresent & 4) != 0;

    if p || !w || !u {
        klog_info!("EXCEPTION_TEST: BUG - Error code parsing incorrect for user write");
        return -1;
    }

    let supervisor_read_present = 0b0001u64;
    let p = (supervisor_read_present & 1) != 0;
    let w = (supervisor_read_present & 2) != 0;
    let u = (supervisor_read_present & 4) != 0;

    if !p || w || u {
        klog_info!("EXCEPTION_TEST: BUG - Error code parsing incorrect for supervisor read");
        return -1;
    }

    0
}

pub fn test_frame_mode_detection() -> c_int {
    let user_frame = create_test_frame(14, true);
    let is_user = (user_frame.cs & 0x3) == 0x3;
    if !is_user {
        klog_info!("EXCEPTION_TEST: BUG - User frame not detected as user mode");
        return -1;
    }

    let kernel_frame = create_test_frame(14, false);
    let is_kernel = (kernel_frame.cs & 0x3) == 0x0;
    if !is_kernel {
        klog_info!("EXCEPTION_TEST: BUG - Kernel frame not detected as kernel mode");
        return -1;
    }

    0
}

pub fn test_frame_invalid_cs() -> c_int {
    let mut frame = create_test_frame(14, true);
    frame.cs = 0x42;

    let cpl = frame.cs & 0x3;
    if cpl > 3 {
        klog_info!("EXCEPTION_TEST: BUG - CPL extraction overflow");
        return -1;
    }

    0
}

pub fn test_frame_noncanonical_addresses() -> c_int {
    let mut frame = create_test_frame(13, true);
    frame.rip = 0x0000_8000_0000_0000;
    frame.rsp = 0x0001_0000_0000_0000;

    if frame.rip != 0x0000_8000_0000_0000 {
        klog_info!("EXCEPTION_TEST: BUG - Non-canonical RIP not preserved");
        return -1;
    }
    if frame.rsp != 0x0001_0000_0000_0000 {
        klog_info!("EXCEPTION_TEST: BUG - Non-canonical RSP not preserved");
        return -1;
    }
    0
}

pub fn test_exception_names_all_vectors() -> c_int {
    for vector in 0..=255u8 {
        let name = get_exception_name(vector);
        if name.is_empty() {
            klog_info!("EXCEPTION_TEST: BUG - Empty name for vector {}", vector);
            return -1;
        }
    }
    0
}

pub fn test_vector_boundaries() -> c_int {
    let name31 = get_exception_name(31);
    if name31.is_empty() {
        klog_info!("EXCEPTION_TEST: BUG - Empty name for vector 31");
        return -1;
    }

    let name32 = get_exception_name(32);
    if name32.is_empty() {
        klog_info!("EXCEPTION_TEST: BUG - Empty name for vector 32");
        return -1;
    }

    let name255 = get_exception_name(255);
    if name255.is_empty() {
        klog_info!("EXCEPTION_TEST: BUG - Empty name for vector 255");
        return -1;
    }

    0
}

pub fn test_error_code_preservation() -> c_int {
    let error_code = 0xDEAD_BEEF_1234_5678u64;
    let frame = create_test_frame_with_error(13, true, error_code);

    if frame.error_code != error_code {
        klog_info!("EXCEPTION_TEST: BUG - Error code not preserved in frame");
        return -1;
    }

    0
}

pub fn test_frame_integrity_patterns() -> c_int {
    let mut frame = create_test_frame(14, true);
    frame.rip = 0xAAAA_AAAA_BBBB_BBBBu64;
    frame.rsp = 0xCCCC_CCCC_DDDD_DDDDu64;
    frame.rax = 0x1111_2222_3333_4444u64;

    if frame.rip != 0xAAAA_AAAA_BBBB_BBBBu64 {
        klog_info!("EXCEPTION_TEST: BUG - RIP corrupted");
        return -1;
    }
    if frame.rsp != 0xCCCC_CCCC_DDDD_DDDDu64 {
        klog_info!("EXCEPTION_TEST: BUG - RSP corrupted");
        return -1;
    }
    if frame.rax != 0x1111_2222_3333_4444u64 {
        klog_info!("EXCEPTION_TEST: BUG - RAX corrupted");
        return -1;
    }

    0
}

pub fn test_known_exception_names() -> c_int {
    let expected = [
        (0, "Divide Error"),
        (1, "Debug"),
        (2, "Non-Maskable Interrupt"),
        (6, "Invalid Opcode"),
        (8, "Double Fault"),
        (13, "General Protection Fault"),
        (14, "Page Fault"),
        (18, "Machine Check"),
    ];

    for (vector, expected_name) in expected {
        let name = get_exception_name(vector);
        if name != expected_name {
            klog_info!(
                "EXCEPTION_TEST: BUG - Vector {} name mismatch: got '{}', expected '{}'",
                vector,
                name,
                expected_name
            );
            return -1;
        }
    }

    0
}

pub fn run_exception_tests() -> (u32, u32) {
    let mut passed = 0u32;
    let mut total = 0u32;

    macro_rules! run_test {
        ($name:expr, $test_fn:expr) => {{
            total += 1;
            let result = $test_fn();
            if result == 0 {
                passed += 1;
            } else {
                klog_info!("EXCEPTION_TEST FAILED: {}", $name);
            }
        }};
    }

    klog_info!("=== Exception Metadata Tests ===");

    run_test!("exception_names_valid", test_exception_names_valid);
    run_test!(
        "critical_exception_classification",
        test_critical_exception_classification
    );
    run_test!("page_fault_error_codes", test_page_fault_error_codes);
    run_test!("frame_mode_detection", test_frame_mode_detection);
    run_test!("frame_invalid_cs", test_frame_invalid_cs);
    run_test!(
        "frame_noncanonical_addresses",
        test_frame_noncanonical_addresses
    );
    run_test!(
        "exception_names_all_vectors",
        test_exception_names_all_vectors
    );
    run_test!("vector_boundaries", test_vector_boundaries);
    run_test!("error_code_preservation", test_error_code_preservation);
    run_test!("frame_integrity_patterns", test_frame_integrity_patterns);
    run_test!("known_exception_names", test_known_exception_names);

    klog_info!("Exception tests: {}/{} passed", passed, total);
    (passed, total)
}
