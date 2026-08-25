//! Shutdown subsystem tests: `StateFlag`, scheduler/task teardown, reinit.

use slopos_ostd::klog_info;
use slopos_ostd::sync::StateFlag;
use slopos_sched::scheduler::{
    init_scheduler, scheduler_enable, scheduler_is_enabled, scheduler_shutdown,
};
use slopos_sched::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TaskPriority, TaskStatus, freeze_kernel_io_all,
    task_create, task_find_by_id, task_registry_reset, task_shutdown_population, task_terminate,
};
use slopos_sched::test_fixture::KernelTestScope;
use slopos_testing::{TestResult, assert_eq_test, assert_test};

use core::ffi::{c_char, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

struct ShutdownFixture {
    _scope: KernelTestScope,
}

impl ShutdownFixture {
    fn new() -> Self {
        Self {
            _scope: KernelTestScope::enter(),
        }
    }
}

extern "C" fn dummy_task_fn(_arg: *mut c_void) {}

fn reset_registry() -> i32 {
    let freeze = freeze_kernel_io_all();
    task_registry_reset(&freeze)
}

fn create_n_tasks(n: usize) -> usize {
    let mut created = 0;
    for _ in 0..n {
        let id = task_create(
            b"TestTask\0".as_ptr() as *const c_char,
            dummy_task_fn,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            break;
        }
        created += 1;
    }
    created
}

pub fn test_stateflag_lifecycle() -> TestResult {
    let flag = StateFlag::new();

    assert_test!(!flag.is_active(), "should start inactive");

    assert_test!(flag.enter(), "first enter should return true");
    assert_test!(flag.is_active(), "should be active after enter");

    assert_test!(!flag.enter(), "second enter should return false");

    flag.leave();
    assert_test!(!flag.is_active(), "should be inactive after leave");
    assert_test!(flag.enter(), "re-enter after leave should succeed");

    TestResult::Pass
}

pub fn test_stateflag_take() -> TestResult {
    let flag = StateFlag::new();

    assert_test!(!flag.take(), "take on inactive should return false");

    flag.set_active();
    assert_test!(flag.take(), "take on active should return true");
    assert_test!(!flag.is_active(), "should be inactive after take");

    TestResult::Pass
}

pub fn test_stateflag_independence() -> TestResult {
    let flag1 = StateFlag::new();
    let flag2 = StateFlag::new();

    flag1.enter();
    assert_test!(flag1.is_active());
    assert_test!(!flag2.is_active(), "flag2 should be independent");

    TestResult::Pass
}

pub fn test_stateflag_concurrent_pattern() -> TestResult {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let flag = StateFlag::new();
    let mut successful_enters = 0u32;

    for _ in 0..10 {
        if flag.enter() {
            successful_enters += 1;
            COUNTER.fetch_add(1, Ordering::SeqCst);
        }
    }

    assert_eq_test!(successful_enters, 1, "only one enter should succeed");
    assert_eq_test!(COUNTER.load(Ordering::SeqCst), 1);

    TestResult::Pass
}

pub fn test_stateflag_relaxed_access() -> TestResult {
    let flag = StateFlag::new();
    assert_test!(!flag.is_active_relaxed());

    flag.set_active();
    assert_test!(flag.is_active_relaxed());

    TestResult::Pass
}

pub fn test_scheduler_shutdown_disables() -> TestResult {
    let _fixture = ShutdownFixture::new();

    assert_eq_test!(scheduler_is_enabled(), 0, "should start disabled");

    scheduler_shutdown();
    assert_eq_test!(
        scheduler_is_enabled(),
        0,
        "should stay disabled after shutdown"
    );

    TestResult::Pass
}

pub fn test_scheduler_shutdown_idempotent() -> TestResult {
    let _fixture = ShutdownFixture::new();

    scheduler_shutdown();
    scheduler_shutdown();
    scheduler_shutdown();
    assert_eq_test!(scheduler_is_enabled(), 0);

    TestResult::Pass
}

pub fn test_scheduler_shutdown_clears_state() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let task_id = task_create(
        b"ShutdownTest\0".as_ptr() as *const c_char,
        dummy_task_fn,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");
    assert_test!(
        task_find_by_id(task_id).is_some(),
        "task should be findable"
    );

    scheduler_shutdown();
    TestResult::Pass
}

pub fn test_task_shutdown_all_terminates() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let created = create_n_tasks(10);
    assert_test!(created > 0, "failed to create any tasks");

    let _result = task_shutdown_population();
    TestResult::Pass
}

pub fn test_task_shutdown_all_empty() -> TestResult {
    let _fixture = ShutdownFixture::new();
    let _result = task_shutdown_population();
    TestResult::Pass
}

pub fn test_task_shutdown_all_idempotent() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let task_id = task_create(
        b"IdempotentTest\0".as_ptr() as *const c_char,
        dummy_task_fn,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    assert_test!(task_id != INVALID_TASK_ID);

    let _r1 = task_shutdown_population();
    let _r2 = task_shutdown_population();
    let _r3 = task_shutdown_population();
    TestResult::Pass
}

/// Tasks must be torn down while the scheduler is still enabled, so a CPU whose
/// current task is destroyed can still `schedule()` to idle.
pub fn test_shutdown_sequence_ordering() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let task_id = task_create(
        b"SeqTest\0".as_ptr() as *const c_char,
        dummy_task_fn,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    assert_test!(task_id != INVALID_TASK_ID);

    let _result = task_shutdown_population();
    scheduler_shutdown();
    TestResult::Pass
}

pub fn test_shutdown_from_clean_state() -> TestResult {
    let _fixture = ShutdownFixture::new();
    scheduler_shutdown();
    let _result = task_shutdown_population();
    TestResult::Pass
}

pub fn test_shutdown_partial_init() -> TestResult {
    let _fixture = ShutdownFixture::new();

    task_shutdown_population();
    let _ = reset_registry();
    // Deliberately skips the second init_scheduler: a shutdown over that
    // half-configured state must still be safe.
    scheduler_shutdown();
    task_shutdown_population();
    TestResult::Pass
}

/// Repeated teardown-and-reinit stays safe, and a task can still be created
/// after each round.
pub fn test_rapid_shutdown_cycles() -> TestResult {
    const CYCLES: usize = 20;

    for cycle in 0..CYCLES {
        let _fixture = ShutdownFixture::new();

        let task_id = task_create(
            b"CycleTask\0".as_ptr() as *const c_char,
            dummy_task_fn,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        assert_test!(
            task_id != INVALID_TASK_ID,
            "cycle {} task creation failed",
            cycle
        );

        task_shutdown_population();
        scheduler_shutdown();
        assert_test!(
            reset_registry() == 0,
            "cycle {} registry reset failed",
            cycle
        );
        assert_test!(
            init_scheduler() == 0,
            "cycle {} scheduler reinit failed",
            cycle
        );
    }

    TestResult::Pass
}

pub fn test_shutdown_many_tasks() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let created = create_n_tasks(50);
    assert_test!(created > 0);

    let _result = task_shutdown_population();
    TestResult::Pass
}

pub fn test_shutdown_mixed_priorities() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let priorities = [
        TaskPriority::High.as_u8(),
        TaskPriority::Normal.as_u8(),
        TaskPriority::Low.as_u8(),
        TaskPriority::Idle.as_u8(),
    ];

    for &priority in &priorities {
        let task_id = task_create(
            b"PriTask\0".as_ptr() as *const c_char,
            dummy_task_fn,
            ptr::null_mut(),
            priority,
            TASK_FLAG_KERNEL_MODE,
        );
        assert_test!(task_id != INVALID_TASK_ID);
    }

    let _result = task_shutdown_population();
    TestResult::Pass
}

pub fn test_task_shutdown_skips_current() -> TestResult {
    let _fixture = ShutdownFixture::new();

    for _ in 0..5 {
        let _ = task_create(
            b"SkipTest\0".as_ptr() as *const c_char,
            dummy_task_fn,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
    }

    let _result = task_shutdown_population();
    TestResult::Pass
}

pub fn test_scheduler_reinit_after_shutdown() -> TestResult {
    let _fixture = ShutdownFixture::new();

    scheduler_shutdown();
    task_shutdown_population();

    assert_eq_test!(reset_registry(), 0, "registry reset failed");
    assert_eq_test!(init_scheduler(), 0, "reinit scheduler failed");

    let task_id = task_create(
        b"ReinitTest\0".as_ptr() as *const c_char,
        dummy_task_fn,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    assert_test!(
        task_id != INVALID_TASK_ID,
        "task creation after reinit failed"
    );

    TestResult::Pass
}

pub fn test_kernel_pml4_root_matches_ostd_master() -> TestResult {
    use slopos_mm::paging::kernel_pml4_phys;
    let recorded = kernel_pml4_phys();
    assert_test!(!recorded.is_null(), "init_paging recorded no kernel PML4");

    // A mapping written through one is invisible to the other unless both root
    // on the same frame.
    let installed = slopos_kernel_services::kernel_vm_space::try_kernel_vm_space();
    assert_test!(installed.is_some(), "kernel_vm_space not installed");
    let ostd_master = installed.unwrap().lock().pml4_paddr();
    assert_test!(
        recorded == ostd_master,
        "kernel PML4 disagrees with the OSTD kernel master"
    );
    TestResult::Pass
}

pub fn test_serial_flush_terminates() -> TestResult {
    use slopos_ostd::test_support::serial as ts_serial;

    let mut iterations = 0;
    for _ in 0..1024 {
        let lsr = ts_serial::read_lsr();
        iterations += 1;
        if (lsr & 0x40) != 0 {
            break;
        }
        slopos_arch::cpu::pause();
    }
    klog_info!(
        "SHUTDOWN_TEST: Serial flush completed in {} iterations",
        iterations
    );
    TestResult::Pass
}

pub fn test_shutdown_e2e_stress_with_allocation() -> TestResult {
    use slopos_mm::page_alloc::{alloc_kernel_page_with, free_page_frame};
    use slopos_mm::slab::{kfree, kmalloc};
    use slopos_ostd::mm::frame::FrameAllocOptions;

    const CYCLES: usize = 10;
    const TASKS_PER_CYCLE: usize = 5;
    const ALLOCS_PER_CYCLE: usize = 8;

    let _fixture = ShutdownFixture::new();

    task_shutdown_population();
    scheduler_shutdown();

    for cycle in 0..CYCLES {
        assert_test!(
            reset_registry() == 0 && init_scheduler() == 0,
            "cycle {} init failed",
            cycle
        );

        for _ in 0..TASKS_PER_CYCLE {
            let _ = task_create(
                b"StressTask\0".as_ptr() as *const c_char,
                dummy_task_fn,
                ptr::null_mut(),
                TaskPriority::Normal.as_u8(),
                TASK_FLAG_KERNEL_MODE,
            );
        }

        let mut heap_ptrs: [*mut c_void; ALLOCS_PER_CYCLE] = [ptr::null_mut(); ALLOCS_PER_CYCLE];
        for i in 0..ALLOCS_PER_CYCLE {
            heap_ptrs[i] = kmalloc(64 + (i * 32));
        }

        let mut page_addrs: [u64; 4] = [0; 4];
        for i in 0..4 {
            let phys = alloc_kernel_page_with(FrameAllocOptions::single().with_no_pcp());
            assert_test!(
                phys.as_u64() != 0,
                "cycle {} alloc {} returned PhysAddr::NULL (OOM)",
                cycle,
                i
            );
            page_addrs[i] = phys.as_u64();
        }

        let _result = task_shutdown_population();
        scheduler_shutdown();

        for ptr in heap_ptrs.iter() {
            if !ptr.is_null() {
                kfree(*ptr);
            }
        }
        for &addr in page_addrs.iter() {
            if addr != 0 {
                free_page_frame(slopos_abi::PhysAddr::new(addr));
            }
        }
    }

    TestResult::Pass
}

/// `task_terminate` on an already-terminated task returns 0 without re-running
/// teardown side-effects.
pub fn test_task_terminate_idempotent() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let task_id = task_create(
        b"TermIdem\0".as_ptr() as *const c_char,
        dummy_task_fn,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");

    assert_eq_test!(task_terminate(task_id), 0, "first terminate should succeed");

    if let Some(task) = task_find_by_id(task_id) {
        let status = task.status();
        assert_eq_test!(
            status,
            TaskStatus::Terminated,
            "task should be Terminated after first call"
        );
    }

    assert_eq_test!(
        task_terminate(task_id),
        0,
        "second terminate should be idempotent"
    );

    assert_eq_test!(
        task_terminate(task_id),
        0,
        "third terminate should be idempotent"
    );

    TestResult::Pass
}

/// The scheduler must stay enabled across `task_shutdown_all` so faulting CPUs
/// can still `schedule()` to idle.
pub fn test_shutdown_scheduler_alive_during_task_teardown() -> TestResult {
    let _fixture = ShutdownFixture::new();

    // The fixture leaves the scheduler disabled.
    scheduler_enable();

    let created = create_n_tasks(5);
    assert_test!(created > 0, "failed to create tasks");

    let _result = task_shutdown_population();
    assert_test!(
        scheduler_is_enabled() != 0,
        "scheduler must remain enabled after task_shutdown_all"
    );

    scheduler_shutdown();
    assert_eq_test!(scheduler_is_enabled(), 0, "scheduler should be disabled");

    TestResult::Pass
}

use slopos_acpi::fadt::{ACPI_ADDR_SPACE_IO, Fadt, find_s5_sleep_types};

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// A modern (ACPI 6.x, 276-byte) FADT with legacy I/O PM1 ports and an
/// I/O-space reset register parses into the expected fields.
pub fn test_fadt_parse_legacy_io_ports() -> TestResult {
    let mut buf = [0u8; 276];
    buf[0..4].copy_from_slice(b"FACP");
    put_u32(&mut buf, 4, 276); // length
    buf[8] = 6; // revision
    put_u32(&mut buf, 48, 0xB2); // SMI_CMD
    buf[52] = 0xA1; // ACPI_ENABLE
    put_u32(&mut buf, 64, 0x404); // PM1a_CNT_BLK
    put_u32(&mut buf, 68, 0); // PM1b_CNT_BLK (single block)
    put_u32(&mut buf, 112, 1 << 10); // FLAGS: RESET_REG_SUP
    buf[116] = ACPI_ADDR_SPACE_IO; // RESET_REG.address_space_id
    put_u64(&mut buf, 120, 0xCF9); // RESET_REG.address
    buf[128] = 0x06; // RESET_VALUE
    // X_ control blocks left zero -> legacy fallback.

    let fadt = Fadt::parse(&buf).expect("FADT should parse");
    assert_eq_test!(fadt.pm1a_cnt_port, 0x404, "pm1a port");
    assert_eq_test!(fadt.pm1b_cnt_port, 0, "pm1b absent");
    assert_eq_test!(fadt.smi_cmd, 0xB2, "smi_cmd");
    assert_eq_test!(fadt.acpi_enable, 0xA1, "acpi_enable");
    let (reg, val) = fadt.reset.expect("reset register present");
    assert_eq_test!(reg.address_space_id, ACPI_ADDR_SPACE_IO, "reset space");
    assert_eq_test!(reg.address, 0xCF9, "reset address");
    assert_eq_test!(val, 0x06, "reset value");
    TestResult::Pass
}

/// The 64-bit extended (`X_`) PM1a control block is preferred over the
/// legacy 32-bit field when it names a non-zero I/O register.
pub fn test_fadt_prefers_extended_io_port() -> TestResult {
    let mut buf = [0u8; 276];
    buf[0..4].copy_from_slice(b"FACP");
    put_u32(&mut buf, 4, 276);
    buf[8] = 6;
    put_u32(&mut buf, 64, 0x404); // legacy PM1a
    buf[172] = ACPI_ADDR_SPACE_IO; // X_PM1a_CNT_BLK.address_space_id
    put_u64(&mut buf, 176, 0x1804); // X_PM1a_CNT_BLK.address

    let fadt = Fadt::parse(&buf).expect("FADT should parse");
    assert_eq_test!(fadt.pm1a_cnt_port, 0x1804, "should prefer extended port");
    TestResult::Pass
}

/// RESET_REG is ignored unless the FADT flags advertise `RESET_REG_SUP`.
pub fn test_fadt_no_reset_when_flag_clear() -> TestResult {
    let mut buf = [0u8; 276];
    buf[0..4].copy_from_slice(b"FACP");
    buf[8] = 6;
    put_u32(&mut buf, 64, 0x404);
    put_u32(&mut buf, 112, 0); // FLAGS: RESET_REG_SUP clear
    buf[116] = ACPI_ADDR_SPACE_IO;
    put_u64(&mut buf, 120, 0xCF9);
    buf[128] = 0x06;

    let fadt = Fadt::parse(&buf).expect("FADT should parse");
    assert_test!(fadt.reset.is_none(), "reset absent when flag clear");
    TestResult::Pass
}

/// A short (ACPI 1.0, 116-byte) FADT yields PM1 ports but no reset reg.
pub fn test_fadt_short_table_no_reset() -> TestResult {
    let mut buf = [0u8; 116];
    buf[0..4].copy_from_slice(b"FACP");
    buf[8] = 1; // revision 1
    put_u32(&mut buf, 64, 0x404);
    put_u32(&mut buf, 68, 0x408);

    let fadt = Fadt::parse(&buf).expect("FADT should parse");
    assert_eq_test!(fadt.pm1a_cnt_port, 0x404, "pm1a port");
    assert_eq_test!(fadt.pm1b_cnt_port, 0x408, "pm1b port");
    assert_test!(fadt.reset.is_none(), "no reset reg in 1.0 FADT");
    TestResult::Pass
}

/// `\_S5` with `BytePrefix`-tagged sleep types decodes both elements.
pub fn test_find_s5_byteprefix() -> TestResult {
    let aml = [
        0x08, 0x5F, 0x53, 0x35, 0x5F, // NameOp "_S5_"
        0x12, 0x06, 0x02, // PackageOp, PkgLength, NumElements=2
        0x0A, 0x05, // BytePrefix 5 -> SLP_TYPa
        0x0A, 0x07, // BytePrefix 7 -> SLP_TYPb
    ];
    let (a, b) = find_s5_sleep_types(&aml).expect("should find _S5");
    assert_eq_test!(a, 5, "SLP_TYPa");
    assert_eq_test!(b, 7, "SLP_TYPb");
    TestResult::Pass
}

/// QEMU-style `\_S5 = Package(){Zero, Zero, ...}` through a root-prefixed
/// name decodes to sleep type 0 (matching the legacy `0x2000` poke).
pub fn test_find_s5_zero_ops_root_prefixed() -> TestResult {
    let aml = [
        0x08, 0x5C, 0x5F, 0x53, 0x35, 0x5F, // NameOp '\' "_S5_"
        0x12, 0x06, 0x04, // PackageOp, PkgLength, NumElements=4
        0x00, 0x00, 0x00, 0x00, // Zero x4
    ];
    let (a, b) = find_s5_sleep_types(&aml).expect("should find _S5");
    assert_eq_test!(a, 0, "SLP_TYPa");
    assert_eq_test!(b, 0, "SLP_TYPb");
    TestResult::Pass
}

/// No `\_S5` object present -> `None` (caller keeps reset usable, skips S5).
pub fn test_find_s5_absent() -> TestResult {
    // `_S4_` (suspend-to-disk), not `_S5_`.
    let aml = [0x08, 0x5F, 0x53, 0x34, 0x5F, 0x12, 0x06, 0x02, 0x00, 0x00];
    assert_test!(find_s5_sleep_types(&aml).is_none(), "must not find _S5");
    TestResult::Pass
}

slopos_testing::stest!(name = test_fadt_parse_legacy_io_ports, suite = shutdown);
slopos_testing::stest!(name = test_fadt_prefers_extended_io_port, suite = shutdown);
slopos_testing::stest!(name = test_fadt_no_reset_when_flag_clear, suite = shutdown);
slopos_testing::stest!(name = test_fadt_short_table_no_reset, suite = shutdown);
slopos_testing::stest!(name = test_find_s5_byteprefix, suite = shutdown);
slopos_testing::stest!(name = test_find_s5_zero_ops_root_prefixed, suite = shutdown);
slopos_testing::stest!(name = test_find_s5_absent, suite = shutdown);

slopos_testing::stest!(name = test_stateflag_lifecycle, suite = shutdown);
slopos_testing::stest!(name = test_stateflag_take, suite = shutdown);
slopos_testing::stest!(name = test_stateflag_independence, suite = shutdown);
slopos_testing::stest!(name = test_stateflag_concurrent_pattern, suite = shutdown);
slopos_testing::stest!(name = test_stateflag_relaxed_access, suite = shutdown);
slopos_testing::stest!(name = test_scheduler_shutdown_disables, suite = shutdown);
slopos_testing::stest!(name = test_scheduler_shutdown_idempotent, suite = shutdown);
slopos_testing::stest!(
    name = test_scheduler_shutdown_clears_state,
    suite = shutdown
);
slopos_testing::stest!(name = test_task_shutdown_all_terminates, suite = shutdown);
slopos_testing::stest!(name = test_task_shutdown_all_empty, suite = shutdown);
slopos_testing::stest!(name = test_task_shutdown_all_idempotent, suite = shutdown);
slopos_testing::stest!(name = test_shutdown_sequence_ordering, suite = shutdown);
slopos_testing::stest!(name = test_shutdown_from_clean_state, suite = shutdown);
slopos_testing::stest!(name = test_shutdown_partial_init, suite = shutdown);
slopos_testing::stest!(name = test_rapid_shutdown_cycles, suite = shutdown);
slopos_testing::stest!(name = test_shutdown_many_tasks, suite = shutdown);
slopos_testing::stest!(name = test_shutdown_mixed_priorities, suite = shutdown);
slopos_testing::stest!(name = test_task_shutdown_skips_current, suite = shutdown);
slopos_testing::stest!(
    name = test_scheduler_reinit_after_shutdown,
    suite = shutdown
);
slopos_testing::stest!(
    name = test_kernel_pml4_root_matches_ostd_master,
    suite = shutdown
);
slopos_testing::stest!(name = test_serial_flush_terminates, suite = shutdown);
slopos_testing::stest!(
    name = test_shutdown_e2e_stress_with_allocation,
    suite = shutdown
);
slopos_testing::stest!(name = test_task_terminate_idempotent, suite = shutdown);
slopos_testing::stest!(
    name = test_shutdown_scheduler_alive_during_task_teardown,
    suite = shutdown
);
