#![no_std]

use core::ffi::{c_char, c_int};
use core::ptr;

use slopos_drivers::interrupt_test as intr;
pub use slopos_drivers::interrupts::{InterruptTestConfig, Verbosity as InterruptTestVerbosity};
use slopos_drivers::interrupts::{SUITE_BASIC, SUITE_CONTROL, SUITE_MEMORY, SUITE_SCHEDULER};
use slopos_drivers::wl_currency;
use slopos_lib::klog_info;

pub const TESTS_MAX_SUITES: usize = 8;
const TESTS_MAX_CYCLES_PER_MS: u64 = 3_000_000;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TestSuiteResult {
    pub name: *const c_char,
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub exceptions_caught: u32,
    pub unexpected_exceptions: u32,
    pub elapsed_ms: u32,
    pub timed_out: c_int,
}

impl Default for TestSuiteResult {
    fn default() -> Self {
        Self {
            name: ptr::null(),
            total: 0,
            passed: 0,
            failed: 0,
            exceptions_caught: 0,
            unexpected_exceptions: 0,
            elapsed_ms: 0,
            timed_out: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TestSuiteDesc {
    pub name: *const c_char,
    pub mask_bit: u32,
    pub run: Option<fn(*const InterruptTestConfig, *mut TestSuiteResult) -> i32>,
}

unsafe impl Sync for TestSuiteDesc {}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TestRunSummary {
    pub suites: [TestSuiteResult; TESTS_MAX_SUITES],
    pub suite_count: usize,
    pub total_tests: u32,
    pub passed: u32,
    pub failed: u32,
    pub exceptions_caught: u32,
    pub unexpected_exceptions: u32,
    pub elapsed_ms: u32,
    pub timed_out: c_int,
}

impl Default for TestRunSummary {
    fn default() -> Self {
        Self {
            suites: [TestSuiteResult::default(); TESTS_MAX_SUITES],
            suite_count: 0,
            total_tests: 0,
            passed: 0,
            failed: 0,
            exceptions_caught: 0,
            unexpected_exceptions: 0,
            elapsed_ms: 0,
            timed_out: 0,
        }
    }
}

static mut REGISTRY: [Option<&'static TestSuiteDesc>; TESTS_MAX_SUITES] = [None; TESTS_MAX_SUITES];
static mut REGISTRY_COUNT: usize = 0;
static mut CACHED_CYCLES_PER_MS: u64 = 0;

fn registry_mut() -> *mut [Option<&'static TestSuiteDesc>; TESTS_MAX_SUITES] {
    &raw mut REGISTRY
}

fn registry_count_mut() -> *mut usize {
    &raw mut REGISTRY_COUNT
}

fn cached_cycles_per_ms_mut() -> *mut u64 {
    &raw mut CACHED_CYCLES_PER_MS
}

fn estimate_cycles_per_ms() -> u64 {
    unsafe {
        if *cached_cycles_per_ms_mut() != 0 {
            return *cached_cycles_per_ms_mut();
        }
    }

    let (max_leaf, _, _, _) = slopos_lib::cpu::cpuid(0);
    let mut cycles_per_ms = TESTS_MAX_CYCLES_PER_MS;
    if max_leaf >= 0x16 {
        let (freq_mhz, _, _, _) = slopos_lib::cpu::cpuid(0x16);
        if freq_mhz != 0 {
            cycles_per_ms = freq_mhz as u64 * 1_000;
        }
    }

    unsafe {
        *cached_cycles_per_ms_mut() = cycles_per_ms;
    }
    cycles_per_ms
}

fn cycles_to_ms(cycles: u64) -> u32 {
    let cycles_per_ms = estimate_cycles_per_ms();
    if cycles_per_ms == 0 {
        return 0;
    }
    let ms = cycles / cycles_per_ms;
    if ms > u32::MAX as u64 {
        return u32::MAX;
    }
    ms as u32
}

fn fill_summary_from_result(summary: &mut TestRunSummary, res: &TestSuiteResult) {
    summary.total_tests = summary.total_tests.saturating_add(res.total);
    summary.passed = summary.passed.saturating_add(res.passed);
    summary.failed = summary.failed.saturating_add(res.failed);
    summary.exceptions_caught = summary
        .exceptions_caught
        .saturating_add(res.exceptions_caught);
    summary.unexpected_exceptions = summary
        .unexpected_exceptions
        .saturating_add(res.unexpected_exceptions);
    summary.elapsed_ms = summary.elapsed_ms.saturating_add(res.elapsed_ms);
    if res.timed_out != 0 {
        summary.timed_out = 1;
    }
}

fn award_wl_for_result(res: &TestSuiteResult) {
    if res.total == 0 {
        return;
    }
    if res.failed == 0 && res.timed_out == 0 {
        wl_currency::award_win();
    } else {
        wl_currency::award_loss();
    }
}
pub fn tests_reset_registry() {
    unsafe {
        (*registry_mut()).iter_mut().for_each(|slot| *slot = None);
        *registry_count_mut() = 0;
    }
}
pub fn tests_register_suite(desc: *const TestSuiteDesc) -> i32 {
    if desc.is_null() {
        return -1;
    }
    let desc_ref = unsafe { &*desc };
    if desc_ref.run.is_none() {
        return -1;
    }
    unsafe {
        if *registry_count_mut() >= TESTS_MAX_SUITES {
            return -1;
        }
        (*registry_mut())[*registry_count_mut()] = Some(desc_ref);
        *registry_count_mut() += 1;
    }
    0
}
pub fn tests_register_system_suites() {
    suites::register_system_suites();
}
pub fn tests_run_all(config: *const InterruptTestConfig, summary: *mut TestRunSummary) -> i32 {
    if config.is_null() {
        return -1;
    }

    let mut local_summary = TestRunSummary::default();
    let summary = if summary.is_null() {
        &mut local_summary
    } else {
        unsafe {
            *summary = TestRunSummary::default();
            &mut *summary
        }
    };

    let cfg = unsafe { &*config };
    if !cfg.enabled {
        klog_info!("TESTS: Harness disabled\n");
        return 0;
    }

    klog_info!("TESTS: Starting orchestrated suites\n");

    let mut desc_list: [Option<&'static TestSuiteDesc>; TESTS_MAX_SUITES] =
        [None; TESTS_MAX_SUITES];
    let mut desc_count = unsafe { *registry_count_mut() };
    if desc_count > TESTS_MAX_SUITES {
        desc_count = TESTS_MAX_SUITES;
    }
    for i in 0..desc_count {
        desc_list[i] = unsafe { (*registry_mut())[i] };
    }

    let start_cycles = slopos_lib::tsc::rdtsc();
    for (idx, entry) in desc_list.iter().enumerate().take(desc_count) {
        let Some(desc) = entry else { continue };

        if (cfg.suite_mask & desc.mask_bit) == 0 {
            continue;
        }

        let mut res = TestSuiteResult::default();
        res.name = desc.name;

        if let Some(run) = desc.run {
            let _ = run(cfg, &mut res);
        }
        award_wl_for_result(&res);

        if summary.suite_count < TESTS_MAX_SUITES {
            summary.suites[summary.suite_count] = res;
            summary.suite_count += 1;
        }

        klog_info!(
            "SUITE{} total={} pass={} fail={} exc={} unexp={} elapsed={} timeout={}\n",
            idx as u32,
            res.total,
            res.passed,
            res.failed,
            res.exceptions_caught,
            res.unexpected_exceptions,
            res.elapsed_ms,
            if res.timed_out != 0 { 1u32 } else { 0u32 },
        );
        fill_summary_from_result(summary, &res);
    }
    let end_cycles = slopos_lib::tsc::rdtsc();
    let overall_ms = cycles_to_ms(end_cycles.wrapping_sub(start_cycles));
    if overall_ms > summary.elapsed_ms {
        summary.elapsed_ms = overall_ms;
    }

    klog_info!(
        "+----------------------+-------+-------+-------+-------+-------+---------+-----+\n"
    );
    klog_info!(
        "TESTS SUMMARY: total={} passed={} failed={} exceptions={} unexpected={} elapsed_ms={} timed_out={}\n",
        summary.total_tests,
        summary.passed,
        summary.failed,
        summary.exceptions_caught,
        summary.unexpected_exceptions,
        summary.elapsed_ms,
        if summary.timed_out != 0 { "yes" } else { "no" },
    );

    if summary.failed == 0 { 0 } else { -1 }
}

mod suites {
    use super::*;

    const INTERRUPT_NAME: &[u8] = b"interrupt\0";
    const VM_NAME: &[u8] = b"vm\0";
    const HEAP_NAME: &[u8] = b"heap\0";
    const RAMFS_NAME: &[u8] = b"ramfs\0";
    const PRIVSEP_NAME: &[u8] = b"privsep\0";
    const CTXSWITCH_NAME: &[u8] = b"ctxswitch_regs\0";
    const ROULETTE_NAME: &[u8] = b"roulette\0";
    const ROULETTE_EXEC_NAME: &[u8] = b"roulette_exec\0";
    const VIRTIO_GPU_NAME: &[u8] = b"virtio_gpu\0";
    pub static INTERRUPT_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
        name: INTERRUPT_NAME.as_ptr() as *const c_char,
        mask_bit: SUITE_BASIC | SUITE_MEMORY | SUITE_CONTROL,
        run: Some(run_interrupt_suite),
    };

    fn run_interrupt_suite(config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        if config.is_null() {
            return -1;
        }

        let mut scoped = unsafe { *config };
        scoped.suite_mask &= SUITE_BASIC | SUITE_MEMORY | SUITE_CONTROL;

        if scoped.suite_mask == 0 {
            if let Some(out_ref) = unsafe { out.as_mut() } {
                out_ref.name = INTERRUPT_NAME.as_ptr() as *const c_char;
            }
            return 0;
        }

        intr::interrupt_test_init(&scoped as *const _);
        intr::run_all_interrupt_tests(&scoped as *const _);
        let stats_ptr = intr::test_get_stats();
        intr::interrupt_test_cleanup();

        let stats = unsafe { stats_ptr.as_ref() };

        if let Some(out_ref) = unsafe { out.as_mut() } {
            out_ref.name = INTERRUPT_NAME.as_ptr() as *const c_char;
            if let Some(s) = stats {
                out_ref.total = s.total_cases;
                out_ref.passed = s.passed_cases;
                out_ref.failed = s.failed_cases;
                out_ref.exceptions_caught = s.exceptions_caught;
                out_ref.unexpected_exceptions = s.unexpected_exceptions;
                out_ref.elapsed_ms = s.elapsed_ms;
                out_ref.timed_out = s.timed_out;
            }
        }

        match stats {
            Some(s) if s.failed_cases == 0 && s.timed_out == 0 => 0,
            Some(_) => -1,
            None => -1,
        }
    }

    #[cfg(feature = "builtin-tests")]
    fn measure_elapsed_ms(start: u64, end: u64) -> u32 {
        super::cycles_to_ms(end.wrapping_sub(start))
    }

    #[cfg(feature = "builtin-tests")]
    fn fill_simple_result(
        out: *mut TestSuiteResult,
        name: &[u8],
        total: u32,
        passed: u32,
        elapsed_ms: u32,
    ) {
        if let Some(out_ref) = unsafe { out.as_mut() } {
            out_ref.name = name.as_ptr() as *const c_char;
            out_ref.total = total;
            out_ref.passed = passed;
            out_ref.failed = total.saturating_sub(passed);
            out_ref.exceptions_caught = 0;
            out_ref.unexpected_exceptions = 0;
            out_ref.elapsed_ms = elapsed_ms;
            out_ref.timed_out = 0;
        }
    }

    #[cfg(not(feature = "builtin-tests"))]
    fn fill_simple_result(
        out: *mut TestSuiteResult,
        name: &[u8],
        _total: u32,
        _passed: u32,
        _elapsed_ms: u32,
    ) {
        if let Some(out_ref) = unsafe { out.as_mut() } {
            *out_ref = TestSuiteResult {
                name: name.as_ptr() as *const c_char,
                ..TestSuiteResult::default()
            };
        }
    }

    #[cfg(feature = "builtin-tests")]
    use slopos_mm::memory_layout::{mm_get_kernel_heap_start, mm_get_process_layout};
    #[cfg(feature = "builtin-tests")]
    use slopos_mm::paging::paging_is_user_accessible;
    #[cfg(feature = "builtin-tests")]
    use slopos_mm::process_vm::{create_process_vm, destroy_process_vm, process_vm_get_page_dir};
    #[cfg(feature = "builtin-tests")]
    use slopos_mm::tests::{
        test_heap_fragmentation_behind_head, test_heap_free_list_search,
        test_process_vm_counter_reset, test_process_vm_slot_reuse,
    };

    #[cfg(feature = "builtin-tests")]
    #[repr(C)]
    struct ProcessMemoryLayout {
        code_start: u64,
        data_start: u64,
        heap_start: u64,
        heap_max: u64,
        stack_top: u64,
        stack_size: u64,
        user_space_start: u64,
        user_space_end: u64,
    }

    #[cfg(feature = "builtin-tests")]
    #[repr(C)]
    struct ProcessPageDir {
        _opaque: [u8; 0],
    }

    #[cfg(feature = "builtin-tests")]
    fn c_str_eq(a: *const c_char, b: *const c_char) -> bool {
        if a.is_null() || b.is_null() {
            return false;
        }
        let mut idx = 0;
        unsafe {
            loop {
                let lhs = *a.add(idx);
                let rhs = *b.add(idx);
                if lhs != rhs {
                    return false;
                }
                if lhs == 0 {
                    return true;
                }
                idx += 1;
            }
        }
    }

    #[cfg(feature = "builtin-tests")]
    fn run_vm_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        unsafe {
            total += 1;
            if test_process_vm_slot_reuse() == 0 {
                passed += 1;
            }
            total += 1;
            if test_process_vm_counter_reset() == 0 {
                passed += 1;
            }
        }

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, VM_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    #[cfg(not(feature = "builtin-tests"))]
    fn run_vm_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        fill_simple_result(out, VM_NAME, 0, 0, 0);
        0
    }

    #[cfg(feature = "builtin-tests")]
    fn run_heap_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let total = 2u32;

        unsafe {
            if test_heap_free_list_search() == 0 {
                passed += 1;
            }
            if test_heap_fragmentation_behind_head() == 0 {
                passed += 1;
            }
        }

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, HEAP_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    #[cfg(not(feature = "builtin-tests"))]
    fn run_heap_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        fill_simple_result(out, HEAP_NAME, 0, 0, 0);
        0
    }

    #[cfg(feature = "builtin-tests")]
    fn run_ramfs_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let total = 5u32;
        let passed = slopos_fs::tests::run_ramfs_tests().max(0) as u32;
        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, RAMFS_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    #[cfg(not(feature = "builtin-tests"))]
    fn run_ramfs_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        fill_simple_result(out, RAMFS_NAME, 0, 0, 0);
        0
    }

    #[cfg(feature = "builtin-tests")]
    fn run_privsep_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let result = slopos_sched::run_privilege_separation_invariant_test();
        let passed = if result == 0 { 1 } else { 0 };
        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, PRIVSEP_NAME, 1, passed, elapsed);
        if result == 0 { 0 } else { -1 }
    }

    #[cfg(not(feature = "builtin-tests"))]
    fn run_privsep_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        fill_simple_result(out, PRIVSEP_NAME, 0, 0, 0);
        0
    }

    fn run_context_switch_regression(
        _config: *const InterruptTestConfig,
        out: *mut TestSuiteResult,
    ) -> i32 {
        fill_simple_result(out, CTXSWITCH_NAME, 0, 0, 0);
        0
    }

    #[cfg(feature = "builtin-tests")]
    fn run_roulette_mapping_suite(
        _config: *const InterruptTestConfig,
        out: *mut TestSuiteResult,
    ) -> i32 {
        let start = slopos_lib::tsc::rdtsc();

        let layout = unsafe { mm_get_process_layout() };
        let stack_probe = if layout.is_null() {
            0
        } else {
            unsafe { (*layout).stack_top }.saturating_sub(0x10)
        };
        let heap_probe = unsafe { mm_get_kernel_heap_start() };

        let total = 3u32;
        let mut passed = 0u32;

        let pid = unsafe { create_process_vm() };
        if pid != u32::MAX {
            let dir = unsafe { process_vm_get_page_dir(pid) };
            if !dir.is_null() {
                let code_ok =
                    unsafe { paging_is_user_accessible(dir, intr::run_all_interrupt_tests as u64) }
                        != 0;
                let stack_ok =
                    layout.is_null() || unsafe { paging_is_user_accessible(dir, stack_probe) } != 0;
                let heap_guard = unsafe { paging_is_user_accessible(dir, heap_probe) } == 0;
                passed = (code_ok as u32) + (stack_ok as u32) + (heap_guard as u32);
            }
            unsafe {
                let _ = destroy_process_vm(pid);
            }
        }

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, ROULETTE_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    #[cfg(not(feature = "builtin-tests"))]
    fn run_roulette_mapping_suite(
        _config: *const InterruptTestConfig,
        out: *mut TestSuiteResult,
    ) -> i32 {
        fill_simple_result(out, ROULETTE_NAME, 0, 0, 0);
        0
    }

    #[cfg(feature = "builtin-tests")]
    fn run_roulette_exec_suite(
        _config: *const InterruptTestConfig,
        out: *mut TestSuiteResult,
    ) -> i32 {
        use slopos_sched::{
            INVALID_TASK_ID, TASK_FLAG_USER_MODE, TASK_STATE_READY, Task, schedule_task,
            task_create, task_get_info, task_terminate,
        };

        let start = slopos_lib::tsc::rdtsc();
        let total = 1u32;
        let mut passed = 0u32;

        let tid = unsafe {
            task_create(
                b"roulette-test\0".as_ptr() as *const c_char,
                slopos_userland::roulette::roulette_user_main,
                ptr::null_mut(),
                5,
                TASK_FLAG_USER_MODE,
            )
        };

        if tid != INVALID_TASK_ID {
            let mut info: *mut Task = ptr::null_mut();
            if unsafe { task_get_info(tid, &mut info) } == 0 && !info.is_null() {
                if unsafe { schedule_task(info) } == 0
                    && unsafe { (*info).state } == TASK_STATE_READY
                {
                    passed = 1;
                }
            }
            unsafe {
                task_terminate(tid);
            }
        }

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, ROULETTE_EXEC_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    #[cfg(not(feature = "builtin-tests"))]
    fn run_roulette_exec_suite(
        _config: *const InterruptTestConfig,
        out: *mut TestSuiteResult,
    ) -> i32 {
        fill_simple_result(out, ROULETTE_EXEC_NAME, 0, 0, 0);
        0
    }

    #[cfg(feature = "builtin-tests")]
    fn run_virtio_gpu_driver_suite(
        _config: *const InterruptTestConfig,
        out: *mut TestSuiteResult,
    ) -> i32 {
        use slopos_drivers::pci::{
            pci_device_info_t, pci_get_registered_driver, pci_get_registered_driver_count,
        };
        use slopos_drivers::virtio_gpu::{
            VIRTIO_GPU_DEVICE_ID_PRIMARY, VIRTIO_GPU_VENDOR_ID, virtio_gpu_register_driver,
        };

        let start = slopos_lib::tsc::rdtsc();

        virtio_gpu_register_driver();

        let mut total = 2u32;
        let mut passed = 0u32;
        let mut virtio_driver: *const slopos_drivers::pci::pci_driver_t = ptr::null();

        let driver_count = pci_get_registered_driver_count();
        for i in 0..driver_count {
            let driver = unsafe { pci_get_registered_driver(i) };
            if driver.is_null() {
                continue;
            }
            if c_str_eq(
                unsafe { (*driver).name as *const c_char },
                b"virtio-gpu\0".as_ptr() as *const c_char,
            ) {
                virtio_driver = driver;
                break;
            }
        }

        if !virtio_driver.is_null() {
            let mut good = pci_device_info_t::default();
            good.vendor_id = VIRTIO_GPU_VENDOR_ID;
            good.device_id = VIRTIO_GPU_DEVICE_ID_PRIMARY;
            unsafe {
                if let Some(m) = (*virtio_driver).match_fn {
                    if m(&good as *const _, (*virtio_driver).context) {
                        passed += 1;
                    }
                }
                let mut bad = pci_device_info_t::default();
                if let Some(m) = (*virtio_driver).match_fn {
                    if !m(&bad as *const _, (*virtio_driver).context) {
                        passed += 1;
                    }
                }
            }
        } else {
            total = 0;
        }

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, VIRTIO_GPU_NAME, total, passed, elapsed);
        if total == 0 || passed == total { 0 } else { -1 }
    }

    #[cfg(not(feature = "builtin-tests"))]
    fn run_virtio_gpu_driver_suite(
        _config: *const InterruptTestConfig,
        out: *mut TestSuiteResult,
    ) -> i32 {
        fill_simple_result(out, VIRTIO_GPU_NAME, 0, 0, 0);
        0
    }

    pub fn register_system_suites() {
        static VM_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: VM_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_vm_suite),
        };
        static HEAP_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: HEAP_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_heap_suite),
        };
        static RAMFS_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: RAMFS_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_ramfs_suite),
        };
        static PRIVSEP_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: PRIVSEP_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_privsep_suite),
        };
        static CTX_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: CTXSWITCH_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_context_switch_regression),
        };
        static ROULETTE_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: ROULETTE_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_roulette_mapping_suite),
        };
        static ROULETTE_EXEC_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: ROULETTE_EXEC_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_roulette_exec_suite),
        };
        static VIRTIO_GPU_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: VIRTIO_GPU_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_virtio_gpu_driver_suite),
        };

        let _ = tests_register_suite(&VM_SUITE_DESC);
        let _ = tests_register_suite(&HEAP_SUITE_DESC);
        let _ = tests_register_suite(&RAMFS_SUITE_DESC);
        let _ = tests_register_suite(&PRIVSEP_SUITE_DESC);
        let _ = tests_register_suite(&CTX_SUITE_DESC);
        let _ = tests_register_suite(&ROULETTE_SUITE_DESC);
        let _ = tests_register_suite(&ROULETTE_EXEC_SUITE_DESC);
        let _ = tests_register_suite(&VIRTIO_GPU_SUITE_DESC);
    }
}

pub use suites::INTERRUPT_SUITE_DESC;
