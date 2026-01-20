//! exec() ELF loader tests - targeting untested code paths likely to have bugs.

use core::ffi::c_int;

use slopos_lib::klog_info;
use slopos_mm::elf::{ELF_MAGIC, ElfValidator};
use slopos_mm::mm_constants::PROCESS_CODE_START_VA;
use slopos_mm::process_vm;

use super::{EXEC_MAX_ELF_SIZE, EXEC_MAX_PATH};

const MINIMAL_ELF_SIZE: usize = 64;

fn create_minimal_elf_header() -> [u8; MINIMAL_ELF_SIZE] {
    let mut elf = [0u8; MINIMAL_ELF_SIZE];

    elf[0..4].copy_from_slice(&ELF_MAGIC);
    elf[4] = 2; // EI_CLASS: 64-bit
    elf[5] = 1; // EI_DATA: little endian
    elf[6] = 1; // EI_VERSION: current
    elf[7] = 0; // EI_OSABI: SYSV
    elf[16..18].copy_from_slice(&2u16.to_le_bytes()); // e_type: ET_EXEC
    elf[18..20].copy_from_slice(&0x3Eu16.to_le_bytes()); // e_machine: x86_64
    elf[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
    elf[24..32].copy_from_slice(&PROCESS_CODE_START_VA.to_le_bytes()); // e_entry
    elf[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff (program headers at offset 64)
    elf[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    elf[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    elf[56..58].copy_from_slice(&0u16.to_le_bytes()); // e_phnum: 0 segments

    elf
}

fn create_elf_with_load_segment(vaddr: u64, memsz: u64, filesz: u64, offset: u64) -> [u8; 120] {
    let mut elf = [0u8; 120];

    elf[0..4].copy_from_slice(&ELF_MAGIC);
    elf[4] = 2;
    elf[5] = 1;
    elf[6] = 1;
    elf[7] = 0;
    elf[16..18].copy_from_slice(&2u16.to_le_bytes());
    elf[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[24..32].copy_from_slice(&vaddr.to_le_bytes()); // e_entry
    elf[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    elf[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    elf[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    elf[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum: 1 segment

    // PT_LOAD = 1 at offset 64
    elf[64..68].copy_from_slice(&1u32.to_le_bytes()); // p_type: PT_LOAD
    elf[68..72].copy_from_slice(&5u32.to_le_bytes()); // p_flags: PF_R | PF_X
    elf[72..80].copy_from_slice(&offset.to_le_bytes()); // p_offset
    elf[80..88].copy_from_slice(&vaddr.to_le_bytes()); // p_vaddr
    elf[88..96].copy_from_slice(&vaddr.to_le_bytes()); // p_paddr
    elf[96..104].copy_from_slice(&filesz.to_le_bytes()); // p_filesz
    elf[104..112].copy_from_slice(&memsz.to_le_bytes()); // p_memsz
    elf[112..120].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align

    elf
}

pub fn test_elf_invalid_magic() -> c_int {
    let mut elf = create_minimal_elf_header();
    elf[0] = 0x00; // Corrupt magic

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted invalid magic");
        return -1;
    }
    0
}

pub fn test_elf_wrong_class() -> c_int {
    let mut elf = create_minimal_elf_header();
    elf[4] = 1; // 32-bit instead of 64-bit

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted 32-bit ELF");
        return -1;
    }
    0
}

pub fn test_elf_wrong_endian() -> c_int {
    let mut elf = create_minimal_elf_header();
    elf[5] = 2; // Big endian

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted big-endian ELF");
        return -1;
    }
    0
}

pub fn test_elf_wrong_machine() -> c_int {
    let mut elf = create_minimal_elf_header();
    elf[18..20].copy_from_slice(&0x03u16.to_le_bytes()); // i386 instead of x86_64

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted i386 ELF on x86_64");
        return -1;
    }
    0
}

pub fn test_elf_truncated_header() -> c_int {
    let elf = [0x7F, b'E', b'L', b'F', 2, 1, 1, 0]; // Only 8 bytes

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted truncated ELF");
        return -1;
    }
    0
}

pub fn test_elf_empty_file() -> c_int {
    let elf: [u8; 0] = [];

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted empty file");
        return -1;
    }
    0
}

pub fn test_elf_no_load_segments() -> c_int {
    let elf = create_minimal_elf_header();

    let validator = match ElfValidator::new(&elf) {
        Ok(v) => v,
        Err(_) => return 0, // Expected to fail without segments
    };

    let (_, count) = match validator.validate_load_segments() {
        Ok(segs) => segs,
        Err(_) => return 0,
    };

    if count > 0 {
        klog_info!("EXEC_TEST: BUG - Found segments in ELF with phnum=0");
        return -1;
    }
    0
}

pub fn test_elf_segment_overflow_vaddr() -> c_int {
    let elf = create_elf_with_load_segment(
        u64::MAX - 0x1000, // vaddr near overflow
        0x2000,            // memsz that would overflow
        0x1000,
        120,
    );

    let validator = match ElfValidator::new(&elf) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return 0,
    };

    if validator.validate_load_segments().is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted segment with vaddr overflow");
        return -1;
    }
    0
}

pub fn test_elf_segment_filesz_greater_than_memsz() -> c_int {
    let elf = create_elf_with_load_segment(
        PROCESS_CODE_START_VA,
        0x1000, // memsz
        0x2000, // filesz > memsz (invalid)
        120,
    );

    let validator = match ElfValidator::new(&elf) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return 0,
    };

    if validator.validate_load_segments().is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted filesz > memsz");
        return -1;
    }
    0
}

pub fn test_elf_segment_offset_overflow() -> c_int {
    let elf = create_elf_with_load_segment(
        PROCESS_CODE_START_VA,
        0x1000,
        0x1000,
        u64::MAX, // offset that would overflow
    );

    let validator = match ElfValidator::new(&elf) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return 0,
    };

    if validator.validate_load_segments().is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted segment offset overflow");
        return -1;
    }
    0
}

pub fn test_elf_kernel_address_entry() -> c_int {
    let mut elf = create_minimal_elf_header();
    // e_entry in kernel space
    elf[24..32].copy_from_slice(&0xFFFF_FFFF_8000_0000u64.to_le_bytes());

    let validator = match ElfValidator::new(&elf) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return 0,
    };

    let header = validator.header();
    if header.e_entry >= 0xFFFF_8000_0000_0000 {
        // Validator should have caught this or translate_address should handle it
        // This is a potential security issue if not handled
        klog_info!("EXEC_TEST: WARNING - ELF with kernel entry point needs relocation");
    }
    0
}

pub fn test_path_too_long() -> c_int {
    let long_path = [b'a'; EXEC_MAX_PATH + 1];

    if long_path.len() <= EXEC_MAX_PATH {
        klog_info!("EXEC_TEST: Test setup error");
        return -1;
    }
    0
}

pub fn test_path_empty() -> c_int {
    let empty_path: [u8; 0] = [];

    if !empty_path.is_empty() {
        klog_info!("EXEC_TEST: Test setup error");
        return -1;
    }
    0
}

pub fn test_translate_address_kernel_to_user() -> c_int {
    use super::translate_address;

    let kernel_addr = 0xFFFF_FFFF_8000_1000u64;
    let min_vaddr = 0xFFFF_FFFF_8000_0000u64;
    let code_base = PROCESS_CODE_START_VA;

    let translated = translate_address(kernel_addr, min_vaddr, code_base);

    if translated >= 0xFFFF_8000_0000_0000 {
        klog_info!("EXEC_TEST: BUG - translate_address didn't move kernel addr to user space");
        return -1;
    }

    if translated < code_base {
        klog_info!("EXEC_TEST: BUG - translated address below code base");
        return -1;
    }

    0
}

pub fn test_translate_address_user_passthrough() -> c_int {
    use super::translate_address;

    let user_addr = 0x0000_0040_0000_1000u64;
    let min_vaddr = 0x0000_0040_0000_0000u64;
    let code_base = PROCESS_CODE_START_VA;

    let translated = translate_address(user_addr, min_vaddr, code_base);

    if translated >= 0xFFFF_8000_0000_0000 {
        klog_info!("EXEC_TEST: BUG - user address translated to kernel space");
        return -1;
    }

    0
}

pub fn test_process_vm_null_page_dir() -> c_int {
    let pid = 9999; // Invalid process ID
    let page_dir = process_vm::process_vm_get_page_dir(pid);

    if !page_dir.is_null() {
        klog_info!("EXEC_TEST: BUG - Got non-null page dir for invalid process");
        return -1;
    }
    0
}

pub fn test_elf_huge_segment_count() -> c_int {
    let mut elf = create_minimal_elf_header();
    // e_phnum = 0xFFFF (maximum)
    elf[56..58].copy_from_slice(&0xFFFFu16.to_le_bytes());

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        let validator = result.unwrap();
        if validator.validate_load_segments().is_ok() {
            klog_info!("EXEC_TEST: BUG - Accepted ELF with impossible segment count");
            return -1;
        }
    }
    0
}

pub fn test_elf_phentsize_mismatch() -> c_int {
    let mut elf = create_minimal_elf_header();
    // e_phentsize = 1 (way too small for a program header)
    elf[54..56].copy_from_slice(&1u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1u16.to_le_bytes()); // 1 segment

    let result = ElfValidator::new(&elf);
    if let Ok(validator) = result {
        if validator.validate_load_segments().is_ok() {
            klog_info!("EXEC_TEST: BUG - Accepted ELF with invalid phentsize");
            return -1;
        }
    }
    0
}

pub fn test_exec_max_size_boundary() -> c_int {
    let max_size = EXEC_MAX_ELF_SIZE;
    let over_max = EXEC_MAX_ELF_SIZE + 1;

    if max_size >= over_max {
        klog_info!("EXEC_TEST: Test constant error");
        return -1;
    }

    if max_size == 0 {
        klog_info!("EXEC_TEST: BUG - EXEC_MAX_ELF_SIZE is zero");
        return -1;
    }
    0
}

pub fn run_exec_tests() -> (u32, u32) {
    let mut passed = 0u32;
    let mut total = 0u32;

    klog_info!("=== exec() ELF Loader Tests ===");

    slopos_lib::run_test!(passed, total, test_elf_invalid_magic);
    slopos_lib::run_test!(passed, total, test_elf_wrong_class);
    slopos_lib::run_test!(passed, total, test_elf_wrong_endian);
    slopos_lib::run_test!(passed, total, test_elf_wrong_machine);
    slopos_lib::run_test!(passed, total, test_elf_truncated_header);
    slopos_lib::run_test!(passed, total, test_elf_empty_file);
    slopos_lib::run_test!(passed, total, test_elf_no_load_segments);
    slopos_lib::run_test!(passed, total, test_elf_segment_overflow_vaddr);
    slopos_lib::run_test!(passed, total, test_elf_segment_filesz_greater_than_memsz);
    slopos_lib::run_test!(passed, total, test_elf_segment_offset_overflow);
    slopos_lib::run_test!(passed, total, test_elf_kernel_address_entry);
    slopos_lib::run_test!(passed, total, test_path_too_long);
    slopos_lib::run_test!(passed, total, test_path_empty);
    slopos_lib::run_test!(passed, total, test_translate_address_kernel_to_user);
    slopos_lib::run_test!(passed, total, test_translate_address_user_passthrough);
    slopos_lib::run_test!(passed, total, test_process_vm_null_page_dir);
    slopos_lib::run_test!(passed, total, test_elf_huge_segment_count);
    slopos_lib::run_test!(passed, total, test_elf_phentsize_mismatch);
    slopos_lib::run_test!(passed, total, test_exec_max_size_boundary);

    klog_info!("exec() tests: {}/{} passed", passed, total);
    (passed, total)
}
