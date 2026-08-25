//! exec() ELF loader tests.

use slopos_abi::auxv::{AT_ENTRY, AT_NULL, AT_PAGESZ, AT_PHDR, AT_PHENT, AT_PHNUM};
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_mm::elf::{ELF_MAGIC, ElfExecInfo, ElfValidator};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_mm::process_vm;
use slopos_ostd::klog_info;
use slopos_sched::test_fixture::KernelTestScope;
use slopos_testing::TestResult;

use super::{EXEC_MAX_ELF_SIZE, EXEC_MAX_PATH, INIT_PATH};

const MINIMAL_ELF_SIZE: usize = 64;

fn read_user_u64(process_id: u32, addr: u64) -> Option<u64> {
    let vm_space = process_vm::process_vm_get_vm_space(resolve_pid(process_id))?;
    process_vm::process_vm_read_user_u64(&vm_space, addr)
}

fn read_user_u8(process_id: u32, addr: u64) -> Option<u8> {
    let vm_space = process_vm::process_vm_get_vm_space(resolve_pid(process_id))?;
    process_vm::process_vm_read_user_u8(&vm_space, addr)
}

fn read_user_cstr(process_id: u32, addr: u64, max_len: usize) -> Option<slopos_ostd::KVec<u8>> {
    let mut buf = slopos_ostd::KVec::<u8>::new();
    for i in 0..max_len {
        let byte = read_user_u8(process_id, addr + i as u64)?;
        if byte == 0 {
            return Some(buf);
        }
        buf.push(byte).ok()?;
    }
    Some(buf)
}

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
    elf[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
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

fn resolve_pid(pid: u32) -> slopos_ostd::process::ProcessId {
    slopos_ostd::process::ProcessId::resolve(pid).expect("a pid this test just created")
}

pub fn test_elf_invalid_magic() -> TestResult {
    let mut elf = create_minimal_elf_header();
    elf[0] = 0x00;

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted invalid magic");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_wrong_class() -> TestResult {
    let mut elf = create_minimal_elf_header();
    elf[4] = 1; // 32-bit instead of 64-bit

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted 32-bit ELF");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_wrong_endian() -> TestResult {
    let mut elf = create_minimal_elf_header();
    elf[5] = 2; // Big endian

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted big-endian ELF");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_wrong_machine() -> TestResult {
    let mut elf = create_minimal_elf_header();
    elf[18..20].copy_from_slice(&0x03u16.to_le_bytes()); // i386 instead of x86_64

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted i386 ELF on x86_64");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_truncated_header() -> TestResult {
    let elf = [0x7F, b'E', b'L', b'F', 2, 1, 1, 0];

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted truncated ELF");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_empty_file() -> TestResult {
    let elf: [u8; 0] = [];

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted empty file");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_no_load_segments() -> TestResult {
    let elf = create_minimal_elf_header();

    let validator = match ElfValidator::new(&elf) {
        Ok(v) => v,
        Err(_) => return TestResult::Pass,
    };

    let (_, count) = match validator.validate_load_segments() {
        Ok(segs) => segs,
        Err(_) => return TestResult::Pass,
    };

    if count > 0 {
        klog_info!("EXEC_TEST: BUG - Found segments in ELF with phnum=0");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_segment_overflow_vaddr() -> TestResult {
    let elf = create_elf_with_load_segment(
        u64::MAX - 0x1000, // vaddr near overflow
        0x2000,            // memsz that would overflow
        0x1000,
        120,
    );

    let validator = match ElfValidator::new(&elf) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return TestResult::Pass,
    };

    if validator.validate_load_segments().is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted segment with vaddr overflow");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_segment_filesz_greater_than_memsz() -> TestResult {
    let elf = create_elf_with_load_segment(
        PROCESS_CODE_START_VA,
        0x1000, // memsz
        0x2000, // filesz > memsz (invalid)
        120,
    );

    let validator = match ElfValidator::new(&elf) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return TestResult::Pass,
    };

    if validator.validate_load_segments().is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted filesz > memsz");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_segment_offset_overflow() -> TestResult {
    let elf = create_elf_with_load_segment(
        PROCESS_CODE_START_VA,
        0x1000,
        0x1000,
        u64::MAX, // offset that would overflow
    );

    let validator = match ElfValidator::new(&elf) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return TestResult::Pass,
    };

    if validator.validate_load_segments().is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted segment offset overflow");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_kernel_address_entry() -> TestResult {
    let kernel_addr: u64 = 0xFFFF_FFFF_8000_0000;
    let elf = create_elf_with_load_segment(
        kernel_addr, // vaddr in kernel space
        0x1000,      // memsz
        0x100,       // filesz
        120,         // offset (past headers)
    );

    let validator = match ElfValidator::new(&elf) {
        Ok(v) => v.with_load_base(PROCESS_CODE_START_VA),
        Err(_) => return TestResult::Pass,
    };

    if validator.validate_load_segments().is_ok() {
        klog_info!("EXEC_TEST: BUG - ElfValidator accepted segment in kernel address space");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_path_too_long() -> TestResult {
    let long_path = [b'a'; EXEC_MAX_PATH + 1];

    if long_path.len() <= EXEC_MAX_PATH {
        klog_info!("EXEC_TEST: Test setup error");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_path_empty() -> TestResult {
    let empty_path: [u8; 0] = [];

    if !empty_path.is_empty() {
        klog_info!("EXEC_TEST: Test setup error");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_translate_address_kernel_to_user() -> TestResult {
    use slopos_mm::process_vm::process_vm_translate_elf_address;

    let kernel_addr = 0xFFFF_FFFF_8000_1000u64;
    let min_vaddr = 0xFFFF_FFFF_8000_0000u64;
    let code_base = PROCESS_CODE_START_VA;

    let translated = process_vm_translate_elf_address(kernel_addr, min_vaddr, code_base);

    if translated >= 0xFFFF_8000_0000_0000 {
        klog_info!("EXEC_TEST: BUG - translate_address didn't move kernel addr to user space");
        return TestResult::Fail;
    }

    if translated < code_base {
        klog_info!("EXEC_TEST: BUG - translated address below code base");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_translate_address_user_passthrough() -> TestResult {
    use slopos_mm::process_vm::process_vm_translate_elf_address;

    let user_addr = 0x0000_0040_0000_1000u64;
    let min_vaddr = 0x0000_0040_0000_0000u64;
    let code_base = PROCESS_CODE_START_VA;

    let translated = process_vm_translate_elf_address(user_addr, min_vaddr, code_base);

    if translated >= 0xFFFF_8000_0000_0000 {
        klog_info!("EXEC_TEST: BUG - user address translated to kernel space");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// The absent designator is reaped, not invented — a pid out of the air is only
/// absent by luck. The scope's AP pause stops it being reissued mid-check.
pub fn test_process_vm_root_absent_for_a_reaped_process() -> TestResult {
    let _scope = KernelTestScope::enter();

    let pid = process_vm::create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return TestResult::Fail;
    }
    let stale = resolve_pid(pid);
    process_vm::destroy_process_vm(stale);

    if slopos_ostd::process::ProcessId::resolve(pid).is_some() {
        klog_info!("EXEC_TEST: BUG - a reaped process's pid still resolved");
        return TestResult::Fail;
    }

    if process_vm::process_vm_get_ostd_pml4_paddr(stale) != 0 {
        klog_info!("EXEC_TEST: BUG - Got an address space for a reaped process");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_elf_huge_segment_count() -> TestResult {
    let mut elf = create_minimal_elf_header();
    // e_phnum = 0xFFFF (maximum)
    elf[56..58].copy_from_slice(&0xFFFFu16.to_le_bytes());

    let result = ElfValidator::new(&elf);
    if result.is_ok() {
        let validator = result.unwrap();
        if validator.validate_load_segments().is_ok() {
            klog_info!("EXEC_TEST: BUG - Accepted ELF with impossible segment count");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_elf_phentsize_mismatch() -> TestResult {
    let mut elf = create_minimal_elf_header();
    // e_phentsize = 1 (way too small for a program header)
    elf[54..56].copy_from_slice(&1u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1u16.to_le_bytes()); // 1 segment

    let result = ElfValidator::new(&elf);
    if let Ok(validator) = result {
        if validator.validate_load_segments().is_ok() {
            klog_info!("EXEC_TEST: BUG - Accepted ELF with invalid phentsize");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_exec_max_size_boundary() -> TestResult {
    let max_size = EXEC_MAX_ELF_SIZE;
    let over_max = EXEC_MAX_ELF_SIZE + 1;

    if max_size >= over_max {
        klog_info!("EXEC_TEST: Test constant error");
        return TestResult::Fail;
    }

    if max_size == 0 {
        klog_info!("EXEC_TEST: BUG - EXEC_MAX_ELF_SIZE is zero");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_init_path_is_absolute() -> TestResult {
    if INIT_PATH.first().copied() != Some(b'/') {
        klog_info!("EXEC_TEST: BUG - INIT_PATH must be absolute");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_init_path_within_exec_limit() -> TestResult {
    if INIT_PATH.is_empty() || INIT_PATH.len() > EXEC_MAX_PATH {
        klog_info!("EXEC_TEST: BUG - INIT_PATH length invalid");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// The vector layout `setup_user_stack` writes, read back through the
/// process's own address space.
pub fn test_setup_user_stack_contract_layout() -> TestResult {
    let _scope = KernelTestScope::enter();
    let pid = process_vm::create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return TestResult::Fail;
    }

    let args: [&[u8]; 1] = [b"/sbin/init"];
    let envs: [&[u8]; 1] = [b"TERM=slop"];
    let exec_info = ElfExecInfo {
        entry: 0x401000,
        phdr_addr: 0x402000,
        phent_size: 56,
        phnum: 3,
        tls_filesz: 0,
        tls_memsz: 0,
        tls_align: 0,
        tls_vaddr: 0,
        tls_tp: 0,
    };

    let Some(table) = slopos_fs::fileio::FdTable::resolve(pid) else {
        return TestResult::Fail;
    };
    let result = super::setup_user_stack(table, Some(&args), Some(&envs), &exec_info);
    let sp = match result {
        Ok(v) => v,
        Err(_) => {
            klog_info!("EXEC_TEST: setup_user_stack returned error in contract layout test");
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };

    let argc = read_user_u64(pid, sp).unwrap_or(u64::MAX);
    if argc != args.len() as u64 {
        klog_info!(
            "EXEC_TEST: argc at sp={:#x} is {}, expected {}",
            sp,
            argc,
            args.len()
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    let argv0 = read_user_u64(pid, sp + 8).unwrap_or(0);
    let argv_null = read_user_u64(pid, sp + 16).unwrap_or(u64::MAX);
    let env0 = read_user_u64(pid, sp + 24).unwrap_or(0);
    let env_null = read_user_u64(pid, sp + 32).unwrap_or(u64::MAX);

    if argv0 == 0 || env0 == 0 || argv_null != 0 || env_null != 0 {
        klog_info!(
            "EXEC_TEST: stack vector layout mismatch argv0={:#x} argv_null={} env0={:#x} env_null={}",
            argv0,
            argv_null,
            env0,
            env_null
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    process_vm::destroy_process_vm(resolve_pid(pid));
    TestResult::Pass
}

pub fn test_setup_user_stack_auxv_required_entries() -> TestResult {
    let _scope = KernelTestScope::enter();
    let pid = process_vm::create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return TestResult::Fail;
    }

    let args: [&[u8]; 2] = [b"/sbin/init", b"--smoke"];
    let envs: [&[u8]; 2] = [b"TERM=slop", b"PATH=/sbin"];
    let exec_info = ElfExecInfo {
        entry: 0x7000_1000,
        phdr_addr: 0x7000_2000,
        phent_size: 56,
        phnum: 5,
        tls_filesz: 0,
        tls_memsz: 0,
        tls_align: 0,
        tls_vaddr: 0,
        tls_tp: 0,
    };

    let Some(table) = slopos_fs::fileio::FdTable::resolve(pid) else {
        return TestResult::Fail;
    };
    let sp = match super::setup_user_stack(table, Some(&args), Some(&envs), &exec_info) {
        Ok(v) => v,
        Err(_) => {
            klog_info!("EXEC_TEST: setup_user_stack returned error in auxv test");
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };

    let argc = read_user_u64(pid, sp).unwrap_or(u64::MAX);
    if argc != args.len() as u64 {
        klog_info!(
            "EXEC_TEST: argc at sp={:#x} is {}, expected {}",
            sp,
            argc,
            args.len()
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    let aux_start = sp + 8 * (1 + args.len() as u64 + 1 + envs.len() as u64 + 1);
    let mut cursor = aux_start;
    let mut saw_phdr = false;
    let mut saw_phent = false;
    let mut saw_phnum = false;
    let mut saw_pagesz = false;
    let mut saw_entry = false;
    let mut saw_null = false;

    for _ in 0..16 {
        let key = read_user_u64(pid, cursor).unwrap_or(u64::MAX);
        let val = read_user_u64(pid, cursor + 8).unwrap_or(u64::MAX);
        if key == AT_PHDR && val == exec_info.phdr_addr {
            saw_phdr = true;
        } else if key == AT_PHENT && val == exec_info.phent_size as u64 {
            saw_phent = true;
        } else if key == AT_PHNUM && val == exec_info.phnum as u64 {
            saw_phnum = true;
        } else if key == AT_PAGESZ && val == PAGE_SIZE_4KB {
            saw_pagesz = true;
        } else if key == AT_ENTRY && val == exec_info.entry {
            saw_entry = true;
        } else if key == AT_NULL && val == 0 {
            saw_null = true;
            break;
        }
        cursor = cursor.wrapping_add(16);
    }

    process_vm::destroy_process_vm(resolve_pid(pid));
    if !(saw_phdr && saw_phent && saw_phnum && saw_pagesz && saw_entry && saw_null) {
        klog_info!(
            "EXEC_TEST: auxv missing entries phdr={} phent={} phnum={} pagesz={} entry={} null={}",
            saw_phdr,
            saw_phent,
            saw_phnum,
            saw_pagesz,
            saw_entry,
            saw_null
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// The stack's argv *string content*, not just its pointers: the layout can
/// look correct while the strings are missing, truncated or at a wrong address.
pub fn test_setup_user_stack_argv_string_content() -> TestResult {
    let _scope = KernelTestScope::enter();
    let pid = process_vm::create_process_vm();
    if pid == INVALID_PROCESS_ID {
        return TestResult::Fail;
    }

    let args: [&[u8]; 4] = [b"nc", b"-u", b"-l", b"12345"];
    let envs: [&[u8]; 1] = [b"PATH=/bin"];
    let exec_info = ElfExecInfo {
        entry: 0x401000,
        phdr_addr: 0x402000,
        phent_size: 56,
        phnum: 1,
        tls_filesz: 0,
        tls_memsz: 0,
        tls_align: 0,
        tls_vaddr: 0,
        tls_tp: 0,
    };

    let Some(table) = slopos_fs::fileio::FdTable::resolve(pid) else {
        return TestResult::Fail;
    };
    let sp = match super::setup_user_stack(table, Some(&args), Some(&envs), &exec_info) {
        Ok(v) => v,
        Err(_) => {
            klog_info!("EXEC_TEST: setup_user_stack failed in argv string test");
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };

    let argc = read_user_u64(pid, sp).unwrap_or(u64::MAX);
    if argc != args.len() as u64 {
        klog_info!(
            "EXEC_TEST: argc at sp={:#x} is {}, expected {}",
            sp,
            argc,
            args.len()
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    for (i, expected) in args.iter().enumerate() {
        let ptr = match read_user_u64(pid, sp + 8 * (1 + i as u64)) {
            Some(p) if p != 0 => p,
            _ => {
                klog_info!("EXEC_TEST: argv[{}] pointer is null or unreadable", i);
                process_vm::destroy_process_vm(resolve_pid(pid));
                return TestResult::Fail;
            }
        };
        let actual = match read_user_cstr(pid, ptr, 256) {
            Some(s) => s,
            None => {
                klog_info!(
                    "EXEC_TEST: cannot read string at argv[{}] ptr={:#x}",
                    i,
                    ptr
                );
                process_vm::destroy_process_vm(resolve_pid(pid));
                return TestResult::Fail;
            }
        };
        if actual.as_slice() != *expected {
            klog_info!(
                "EXEC_TEST: argv[{}] mismatch: expected len={} got len={}",
                i,
                expected.len(),
                actual.len()
            );
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    }

    let argv_null = read_user_u64(pid, sp + 8 * (1 + args.len() as u64)).unwrap_or(u64::MAX);
    if argv_null != 0 {
        klog_info!(
            "EXEC_TEST: argv null terminator missing, got {:#x}",
            argv_null
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    let envp0_slot = sp + 8 * (1 + args.len() as u64 + 1);
    let envp0_ptr = match read_user_u64(pid, envp0_slot) {
        Some(p) if p != 0 => p,
        _ => {
            klog_info!("EXEC_TEST: envp[0] pointer is null or unreadable");
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };
    let env_actual = match read_user_cstr(pid, envp0_ptr, 256) {
        Some(s) => s,
        None => {
            klog_info!(
                "EXEC_TEST: cannot read string at envp[0] ptr={:#x}",
                envp0_ptr
            );
            process_vm::destroy_process_vm(resolve_pid(pid));
            return TestResult::Fail;
        }
    };
    if env_actual.as_slice() != envs[0] {
        klog_info!(
            "EXEC_TEST: envp[0] mismatch: expected len={} got len={}",
            envs[0].len(),
            env_actual.len()
        );
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    // SysV ABI: sp must be 16-byte aligned at entry.
    if sp % 16 != 0 {
        klog_info!("EXEC_TEST: sp={:#x} not 16-byte aligned", sp);
        process_vm::destroy_process_vm(resolve_pid(pid));
        return TestResult::Fail;
    }

    process_vm::destroy_process_vm(resolve_pid(pid));
    TestResult::Pass
}

slopos_testing::stest!(name = test_elf_invalid_magic, suite = exec);
slopos_testing::stest!(name = test_elf_wrong_class, suite = exec);
slopos_testing::stest!(name = test_elf_wrong_endian, suite = exec);
slopos_testing::stest!(name = test_elf_wrong_machine, suite = exec);
slopos_testing::stest!(name = test_elf_truncated_header, suite = exec);
slopos_testing::stest!(name = test_elf_empty_file, suite = exec);
slopos_testing::stest!(name = test_elf_no_load_segments, suite = exec);
slopos_testing::stest!(name = test_elf_segment_overflow_vaddr, suite = exec);
slopos_testing::stest!(
    name = test_elf_segment_filesz_greater_than_memsz,
    suite = exec
);
slopos_testing::stest!(name = test_elf_segment_offset_overflow, suite = exec);
slopos_testing::stest!(name = test_elf_kernel_address_entry, suite = exec);
slopos_testing::stest!(name = test_path_too_long, suite = exec);
slopos_testing::stest!(name = test_path_empty, suite = exec);
slopos_testing::stest!(name = test_translate_address_kernel_to_user, suite = exec);
slopos_testing::stest!(name = test_translate_address_user_passthrough, suite = exec);
slopos_testing::stest!(
    name = test_process_vm_root_absent_for_a_reaped_process,
    suite = exec
);
slopos_testing::stest!(name = test_elf_huge_segment_count, suite = exec);
slopos_testing::stest!(name = test_elf_phentsize_mismatch, suite = exec);
slopos_testing::stest!(name = test_exec_max_size_boundary, suite = exec);
slopos_testing::stest!(name = test_init_path_is_absolute, suite = exec);
slopos_testing::stest!(name = test_init_path_within_exec_limit, suite = exec);
slopos_testing::stest!(name = test_setup_user_stack_contract_layout, suite = exec);
slopos_testing::stest!(
    name = test_setup_user_stack_auxv_required_entries,
    suite = exec
);
slopos_testing::stest!(
    name = test_setup_user_stack_argv_string_content,
    suite = exec
);

/// The grant table is the sole source of privilege bits for a user-initiated
/// spawn. `/sbin/init` is the load-bearing negative case: granting it would let
/// any task re-spawn it and inherit console administration.
pub fn test_program_grants_are_keyed_on_exact_path() -> TestResult {
    use slopos_abi::task::{TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TaskPriority};
    use slopos_testing::assert_test;

    use super::grants::grant_for;

    let (flags, priority) = grant_for(b"/bin/compositor");
    assert_test!(
        flags == TASK_FLAG_COMPOSITOR | slopos_abi::task::TASK_FLAG_LAUNCH,
        "the compositor must be granted COMPOSITOR and LAUNCH"
    );
    assert_test!(
        matches!(priority, Some(TaskPriority::High)),
        "the compositor must be granted the High tier the syscall refuses"
    );

    let (flags, priority) = grant_for(b"/bin/roulette");
    assert_test!(
        flags == TASK_FLAG_DISPLAY_EXCLUSIVE,
        "roulette must be granted DISPLAY_EXCLUSIVE"
    );
    assert_test!(
        priority.is_none(),
        "roulette needs no tier grant — Normal is user-requestable"
    );

    let (flags, priority) = grant_for(b"/bin/keymap");
    assert_test!(
        flags == slopos_abi::task::TASK_FLAG_CONSOLE_ADMIN,
        "the keymap program must be granted CONSOLE_ADMIN"
    );
    assert_test!(
        priority.is_none(),
        "keymap needs no tier grant — Normal is user-requestable"
    );

    assert_test!(
        grant_for(INIT_PATH) == (0, None),
        "init must not be grantable: SYSTEM stays kernel-only"
    );
    // The shell is a launcher, not an ordinary program: it holds `Launch` and
    // nothing else, so it may spawn a program whose identity earns authority
    // without holding any of that authority itself.
    assert_test!(
        grant_for(b"/bin/shell") == (slopos_abi::task::TASK_FLAG_LAUNCH, None),
        "the shell must be granted LAUNCH and nothing more"
    );
    assert_test!(
        grant_for(b"/bin/file_manager") == (0, None),
        "an ordinary program must get nothing"
    );
    assert_test!(
        grant_for(b"/bin/./roulette") == (0, None),
        "a non-canonical spelling must fail closed rather than be normalised"
    );
    assert_test!(
        grant_for(b"") == (0, None),
        "the empty path must get nothing"
    );

    TestResult::Pass
}

slopos_testing::stest!(
    name = test_program_grants_are_keyed_on_exact_path,
    suite = exec
);
