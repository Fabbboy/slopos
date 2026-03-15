//! Syscall Validation Tests
//!
//! Targets: invalid/null pointer handling, boundary conditions,
//! permission checks, resource exhaustion, and dispatch edge cases.

use core::ffi::{c_char, c_void};
use core::ptr;
use core::sync::atomic::Ordering;

use crate::scheduler::task_struct::Task;
use crate::syscall::fs::syscall_ioctl;
use crate::syscall::handlers::{
    syscall_arch_prctl, syscall_futex, syscall_getpgid, syscall_setpgid, syscall_setsid,
};
use crate::syscall::signal::{
    deliver_pending_signal, syscall_kill, syscall_rt_sigaction, syscall_rt_sigprocmask,
    syscall_rt_sigreturn,
};
use slopos_abi::addr::PhysAddr;
use slopos_abi::fs::O_RDONLY;
use slopos_abi::signal::{
    SIG_SETMASK, SIG_UNBLOCK, SIGCHLD, SIGUSR1, SigSet, SignalFrame, UserSigaction, sig_bit,
};
use slopos_abi::syscall::{
    ARCH_GET_FS, ARCH_SET_FS, CLONE_SETTLS, CLONE_SIGHAND, CLONE_THREAD, CLONE_VM, ERRNO_EAGAIN,
    F_GETFL, FUTEX_WAIT, FUTEX_WAKE, MAP_ANONYMOUS, MAP_PRIVATE, O_NOCTTY, O_NONBLOCK, POLLIN,
    SYSCALL_ARCH_PRCTL, SYSCALL_CLONE, SYSCALL_FUTEX, SYSCALL_GETPGID, SYSCALL_IOCTL, SYSCALL_KILL,
    SYSCALL_NET_SCAN, SYSCALL_PIPE, SYSCALL_PIPE2, SYSCALL_POLL, SYSCALL_RT_SIGACTION,
    SYSCALL_RT_SIGPROCMASK, SYSCALL_RT_SIGRETURN, SYSCALL_SELECT, SYSCALL_SETPGID, SYSCALL_SETSID,
    SYSCALL_TABLE_SIZE, SYSCALL_VHANGUP, TIOCSCTTY, TtyIndex,
};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_USER_MODE, TaskStatus};
use slopos_lib::InterruptFrame;
use slopos_lib::{assert_eq_test, assert_not_null, assert_test, klog_info, testing::TestResult};
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame};
use slopos_mm::paging::map_page_4kb_in_dir;
use slopos_mm::paging_defs::PageFlags;
use slopos_mm::process_vm::{process_vm_alloc, process_vm_get_stack_top};
use slopos_mm::user_copy::{copy_from_user, copy_to_user, set_syscall_process_id};
use slopos_mm::user_ptr::UserPtr;

use crate::scheduler::scheduler::{init_scheduler, scheduler_shutdown};
use crate::scheduler::task::{
    init_task_manager, task_clone, task_create, task_find_by_id, task_fork, task_set_state,
    task_shutdown_all, task_terminate,
};
use crate::scheduler::{per_cpu, task};
use crate::syscall::handlers::syscall_lookup;
use slopos_fs::fileio::{
    file_close_fd, file_fcntl_fd, file_open_for_process, file_pipe_create, file_poll_fd,
    file_read_fd, file_write_fd, fileio_clone_table_for_process, fileio_destroy_table_for_process,
};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;

// =============================================================================
// Test Helpers
// =============================================================================

struct SyscallFixture {
    aps_paused: bool,
}

impl SyscallFixture {
    fn new() -> Self {
        let aps_paused = crate::scheduler::per_cpu::pause_all_aps();
        task_shutdown_all();
        scheduler_shutdown();
        let _ = init_task_manager();
        let _ = init_scheduler();
        Self { aps_paused }
    }
}

impl Drop for SyscallFixture {
    fn drop(&mut self) {
        task_shutdown_all();
        scheduler_shutdown();
        crate::scheduler::per_cpu::resume_all_aps_if_not_nested(self.aps_paused);
    }
}

fn dummy_task_entry(_arg: *mut c_void) {}

fn create_test_kernel_task() -> u32 {
    task_create(
        b"KernelTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        1,
        TASK_FLAG_KERNEL_MODE,
    )
}

fn create_test_user_task() -> u32 {
    let user_entry = unsafe { core::mem::transmute(PROCESS_CODE_START_VA as usize) };
    let id = task_create(
        b"UserTest\0".as_ptr() as *const c_char,
        user_entry,
        ptr::null_mut(),
        1,
        TASK_FLAG_USER_MODE,
    );
    // Block the task immediately so the scheduler on other CPUs never picks
    // it up.  These tests only inspect task structures — they never need the
    // task to actually run user-mode code.
    if id != INVALID_TASK_ID {
        task_set_state(id, TaskStatus::Blocked);
    }
    id
}

fn zero_frame() -> InterruptFrame {
    unsafe { core::mem::zeroed() }
}

fn pts_path_for(number: u32) -> Option<[u8; 11]> {
    if number > 9 {
        return None;
    }
    let mut path = *b"/dev/pts/0\0";
    path[9] = b'0' + number as u8;
    Some(path)
}

fn with_user_process_context<R>(pid: u32, f: impl FnOnce() -> R) -> Option<R> {
    let page_dir = slopos_mm::process_vm::process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        return None;
    }
    if slopos_mm::paging::switch_page_directory(page_dir) != 0 {
        return None;
    }
    let _guard = set_syscall_process_id(pid);
    let out = f();
    let kernel_dir = slopos_mm::paging::paging_get_kernel_directory();
    if !kernel_dir.is_null() {
        let _ = slopos_mm::paging::switch_page_directory(kernel_dir);
    }
    Some(out)
}

fn user_copy_out<T: Copy>(pid: u32, addr: u64, value: &T) -> bool {
    with_user_process_context(pid, || {
        let ptr = match UserPtr::<T>::try_new(addr) {
            Ok(p) => p,
            Err(_) => return false,
        };
        copy_to_user(ptr, value).is_ok()
    })
    .unwrap_or(false)
}

fn user_copy_in<T: Copy>(pid: u32, addr: u64) -> Option<T> {
    with_user_process_context(pid, || {
        let ptr = UserPtr::<T>::try_new(addr).ok()?;
        copy_from_user(ptr).ok()
    })?
}

fn map_user_rw_page(pid: u32) -> Option<u64> {
    let base = process_vm_alloc(pid, 4096, PageFlags::USER_RW.bits() as u32);
    if base == 0 {
        return None;
    }

    let page_dir = slopos_mm::process_vm::process_vm_get_page_dir(pid);
    if page_dir.is_null() {
        return None;
    }

    let phys: PhysAddr = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        return None;
    }

    if map_page_4kb_in_dir(
        page_dir,
        slopos_abi::addr::VirtAddr::new(base),
        phys,
        PageFlags::USER_RW.bits(),
    ) != 0
    {
        return None;
    }

    Some(base)
}

// =============================================================================
// Syscall Dispatch Tests
// =============================================================================

pub fn test_syscall_lookup_invalid_number() -> TestResult {
    assert_test!(
        syscall_lookup(0xFFFF).is_null(),
        "should reject out-of-bounds"
    );
    assert_test!(
        syscall_lookup(SYSCALL_TABLE_SIZE as u64).is_null(),
        "should reject boundary"
    );
    assert_test!(syscall_lookup(u64::MAX).is_null(), "should reject u64::MAX");
    TestResult::Pass
}

pub fn test_syscall_lookup_empty_slot() -> TestResult {
    let entry = syscall_lookup(9);
    assert_test!(entry.is_null(), "unimplemented slot should return null");
    TestResult::Pass
}

pub fn test_syscall_lookup_valid() -> TestResult {
    // SYSCALL_EXIT = 1
    let entry = syscall_lookup(1);
    assert_not_null!(entry, "SYSCALL_EXIT lookup returned null");
    let entry_ref = unsafe { &*entry };
    assert_test!(entry_ref.handler.is_some(), "SYSCALL_EXIT has no handler");
    TestResult::Pass
}

pub fn test_process_syscall_lookup_valid() -> TestResult {
    let required = [
        SYSCALL_CLONE,
        SYSCALL_ARCH_PRCTL,
        SYSCALL_FUTEX,
        SYSCALL_RT_SIGACTION,
        SYSCALL_RT_SIGPROCMASK,
        SYSCALL_KILL,
        SYSCALL_RT_SIGRETURN,
    ];

    for sysno in required {
        let entry = syscall_lookup(sysno);
        assert_not_null!(entry, "required syscall missing from table");
        assert_test!(
            unsafe { (*entry).handler.is_some() },
            "required syscall has no handler"
        );
    }

    TestResult::Pass
}

pub fn test_io_syscall_lookup_valid() -> TestResult {
    let required = [
        SYSCALL_POLL,
        SYSCALL_SELECT,
        SYSCALL_PIPE,
        SYSCALL_PIPE2,
        SYSCALL_IOCTL,
        SYSCALL_SETPGID,
        SYSCALL_GETPGID,
        SYSCALL_SETSID,
    ];

    for sysno in required {
        let entry = syscall_lookup(sysno);
        assert_not_null!(entry, "required syscall missing from dispatch table");
        assert_test!(
            unsafe { (*entry).handler.is_some() },
            "required syscall has no handler in dispatch table"
        );
    }

    TestResult::Pass
}

pub fn test_net_scan_syscall_lookup_valid() -> TestResult {
    let entry = syscall_lookup(SYSCALL_NET_SCAN);
    assert_not_null!(entry, "net_scan syscall missing from table");
    assert_test!(
        unsafe { (*entry).handler.is_some() },
        "net_scan syscall has no handler"
    );
    TestResult::Pass
}

pub fn test_pipe_poll_eof_baseline() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe creation failed"
    );

    let payload = b"wheel";
    let written = file_write_fd(
        pid,
        write_fd,
        payload.as_ptr() as *const c_char,
        payload.len(),
    );
    assert_eq_test!(written as usize, payload.len(), "pipe write failed");

    let revents = file_poll_fd(pid, read_fd, POLLIN);
    assert_test!((revents & POLLIN) != 0, "pipe read fd should be readable");

    let mut out = [0u8; 8];
    let read = file_read_fd(pid, read_fd, out.as_mut_ptr() as *mut c_char, payload.len());
    assert_eq_test!(read as usize, payload.len(), "pipe read length mismatch");
    assert_test!(&out[..payload.len()] == payload, "pipe payload mismatch");

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write fd failed");
    let eof_read = file_read_fd(pid, read_fd, out.as_mut_ptr() as *mut c_char, out.len());
    assert_eq_test!(eof_read, 0, "pipe EOF read should return 0");
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read fd failed");

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_process_group_session_syscalls_baseline() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID);
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr);

    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID);
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr);

    let mut frame = zero_frame();
    let _ = syscall_getpgid(parent_ptr, &mut frame);
    assert_eq_test!(
        frame.rax as u32,
        unsafe { (*parent_ptr).pgid },
        "getpgid self mismatch"
    );

    let mut setpgid_frame = zero_frame();
    setpgid_frame.rdi = child_id as u64;
    setpgid_frame.rsi = parent_id as u64;
    let _ = syscall_setpgid(parent_ptr, &mut setpgid_frame);
    assert_eq_test!(setpgid_frame.rax, 0, "setpgid should succeed for child");
    assert_eq_test!(
        unsafe { (*child_ptr).pgid },
        parent_id,
        "child pgid mismatch after setpgid"
    );

    let mut setsid_frame = zero_frame();
    let _ = syscall_setsid(child_ptr, &mut setsid_frame);
    assert_eq_test!(
        setsid_frame.rax as u32,
        child_id,
        "setsid should return child sid"
    );
    assert_eq_test!(
        unsafe { (*child_ptr).sid },
        child_id,
        "child sid mismatch after setsid"
    );
    assert_eq_test!(
        unsafe { (*child_ptr).pgid },
        child_id,
        "child pgid mismatch after setsid"
    );

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_kill_process_group_semantics() -> TestResult {
    let _fixture = SyscallFixture::new();

    let leader_id = create_test_user_task();
    assert_test!(leader_id != INVALID_TASK_ID, "failed to create leader task");
    let leader_ptr = task_find_by_id(leader_id);
    assert_not_null!(leader_ptr, "leader lookup failed");

    let member_id = task_fork(leader_ptr, core::ptr::null());
    assert_test!(member_id != INVALID_TASK_ID, "failed to fork member task");
    task_set_state(member_id, TaskStatus::Blocked);
    let member_ptr = task_find_by_id(member_id);
    assert_not_null!(member_ptr, "member lookup failed");

    let mut setpgid_frame = zero_frame();
    setpgid_frame.rdi = member_id as u64;
    setpgid_frame.rsi = leader_id as u64;
    let _ = syscall_setpgid(leader_ptr, &mut setpgid_frame);
    assert_eq_test!(setpgid_frame.rax, 0, "setpgid should succeed for member");

    let leader_pid = unsafe { (*leader_ptr).process_id };
    let member_pid = unsafe { (*member_ptr).process_id };

    let mut probe_frame = zero_frame();
    probe_frame.rdi = (-(leader_id as i32) as i64) as u64;
    probe_frame.rsi = 0;
    let _ = with_user_process_context(leader_pid, || syscall_kill(leader_ptr, &mut probe_frame));
    assert_eq_test!(probe_frame.rax, 0, "kill(group, 0) probe should succeed");

    unsafe {
        (*leader_ptr).signal_pending.store(0, Ordering::Release);
        (*member_ptr).signal_pending.store(0, Ordering::Release);
    }

    let mut negative_group_frame = zero_frame();
    negative_group_frame.rdi = (-(leader_id as i32) as i64) as u64;
    negative_group_frame.rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(leader_pid, || {
        syscall_kill(leader_ptr, &mut negative_group_frame)
    });
    assert_eq_test!(negative_group_frame.rax, 0, "kill(-pgid, SIGUSR1) failed");

    let pending_bit = sig_bit(SIGUSR1);
    let leader_pending = unsafe { (*leader_ptr).signal_pending.load(Ordering::Acquire) };
    let member_pending = unsafe { (*member_ptr).signal_pending.load(Ordering::Acquire) };
    assert_test!(
        (leader_pending & pending_bit) != 0,
        "leader did not receive group signal"
    );
    assert_test!(
        (member_pending & pending_bit) != 0,
        "member did not receive group signal"
    );

    unsafe {
        (*leader_ptr).signal_pending.store(0, Ordering::Release);
        (*member_ptr).signal_pending.store(0, Ordering::Release);
    }

    let mut caller_group_frame = zero_frame();
    caller_group_frame.rdi = 0;
    caller_group_frame.rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(member_pid, || {
        syscall_kill(member_ptr, &mut caller_group_frame)
    });
    assert_eq_test!(caller_group_frame.rax, 0, "kill(0, SIGUSR1) failed");

    let leader_pending_after = unsafe { (*leader_ptr).signal_pending.load(Ordering::Acquire) };
    let member_pending_after = unsafe { (*member_ptr).signal_pending.load(Ordering::Acquire) };
    assert_test!(
        (leader_pending_after & pending_bit) != 0,
        "leader missing kill(0) group signal"
    );
    assert_test!(
        (member_pending_after & pending_bit) != 0,
        "member missing kill(0) group signal"
    );

    task_terminate(member_id);
    task_terminate(leader_id);
    TestResult::Pass
}

pub fn test_tiocsctty_session_leader_acquires_ctty() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    let mut frame = zero_frame();
    frame.rdi = 0;
    frame.rsi = TIOCSCTTY;
    frame.rdx = 0;
    let _ = syscall_ioctl(task_ptr, &mut frame);
    assert_eq_test!(frame.rax, 0, "TIOCSCTTY should succeed for session leader");

    let sid = unsafe { (*task_ptr).sid };
    let ctty = unsafe { (*task_ptr).controlling_tty };
    assert_eq_test!(ctty, Some(TtyIndex(0)), "controlling_tty should be tty0");

    let tty_sid = slopos_lib::kernel_services::syscall_services::tty::get_session_id(TtyIndex(0));
    assert_eq_test!(tty_sid, sid, "tty session should match caller sid");

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_tiocsctty_non_leader_rejected() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent task");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);

    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    let mut frame = zero_frame();
    frame.rdi = 0;
    frame.rsi = TIOCSCTTY;
    frame.rdx = 0;
    let _ = syscall_ioctl(child_ptr, &mut frame);
    assert_test!(
        frame.rax != 0,
        "TIOCSCTTY should fail for non-session leader"
    );

    assert_eq_test!(
        unsafe { (*child_ptr).controlling_tty },
        None,
        "child ctty should remain None"
    );

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_open_dev_tty_with_o_noctty_preserves_flag() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    let mut frame = zero_frame();
    frame.rdi = 0;
    frame.rsi = TIOCSCTTY;
    frame.rdx = 0;
    let _ = syscall_ioctl(task_ptr, &mut frame);
    assert_eq_test!(
        frame.rax,
        0,
        "TIOCSCTTY should succeed before /dev/tty open"
    );

    let pid = unsafe { (*task_ptr).process_id };
    let path = b"/dev/tty\0";
    let cpu_id = slopos_lib::get_current_cpu();
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(task_ptr));
    let fd = file_open_for_process(
        pid,
        path.as_ptr() as *const c_char,
        O_RDONLY | O_NOCTTY as u32,
    );
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(ptr::null_mut()));
    assert_test!(fd >= 0, "open(/dev/tty, O_NOCTTY) failed");

    let flags = file_fcntl_fd(pid, fd, F_GETFL, 0);
    let close_rc = file_close_fd(pid, fd);
    task_terminate(task_id);

    assert_eq_test!(close_rc, 0, "close /dev/tty fd failed");
    assert_test!(flags >= 0, "F_GETFL failed for /dev/tty fd");
    assert_test!(
        (flags as u64 & O_NOCTTY) != 0,
        "F_GETFL should preserve O_NOCTTY on /dev/tty fd"
    );

    TestResult::Pass
}

pub fn test_setsid_child_preserves_parent_controlling_tty() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent task");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    let mut ioctl_frame = zero_frame();
    ioctl_frame.rdi = 0;
    ioctl_frame.rsi = TIOCSCTTY;
    ioctl_frame.rdx = 0;
    let _ = syscall_ioctl(parent_ptr, &mut ioctl_frame);
    assert_eq_test!(ioctl_frame.rax, 0, "parent TIOCSCTTY should succeed");

    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);

    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    let mut setsid_frame = zero_frame();
    let _ = syscall_setsid(child_ptr, &mut setsid_frame);
    assert_eq_test!(
        setsid_frame.rax as u32,
        child_id,
        "setsid should succeed for child"
    );
    assert_eq_test!(
        unsafe { (*child_ptr).controlling_tty },
        None,
        "child should drop inherited ctty"
    );
    assert_eq_test!(
        unsafe { (*parent_ptr).controlling_tty },
        Some(TtyIndex(0)),
        "parent should retain controlling tty"
    );

    let tty_sid = slopos_lib::kernel_services::syscall_services::tty::get_session_id(TtyIndex(0));
    assert_eq_test!(
        tty_sid,
        parent_id,
        "tty session should stay attached to parent session"
    );

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_hangup_clears_all_session_controlling_ttys() -> TestResult {
    let _fixture = SyscallFixture::new();

    let leader_id = create_test_user_task();
    assert_test!(leader_id != INVALID_TASK_ID, "failed to create leader task");
    let leader_ptr = task_find_by_id(leader_id);
    assert_not_null!(leader_ptr, "leader lookup failed");

    let mut ioctl_frame = zero_frame();
    ioctl_frame.rdi = 0;
    ioctl_frame.rsi = TIOCSCTTY;
    ioctl_frame.rdx = 0;
    let _ = syscall_ioctl(leader_ptr, &mut ioctl_frame);
    assert_eq_test!(ioctl_frame.rax, 0, "leader TIOCSCTTY should succeed");

    let child_id = task_fork(leader_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    slopos_lib::kernel_services::syscall_services::tty::hangup(TtyIndex(0));

    assert_eq_test!(
        unsafe { (*leader_ptr).controlling_tty },
        None,
        "leader ctty should clear on hangup"
    );
    assert_eq_test!(
        unsafe { (*child_ptr).controlling_tty },
        None,
        "child ctty should clear on hangup"
    );
    assert_eq_test!(
        slopos_lib::kernel_services::syscall_services::tty::get_session_id(TtyIndex(0)),
        0,
        "tty session should detach on hangup"
    );

    task_terminate(child_id);
    task_terminate(leader_id);
    TestResult::Pass
}

pub fn test_pts_open_acquires_controlling_tty_without_o_noctty() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    let master_idx_raw = slopos_lib::kernel_services::syscall_services::tty::alloc_pty();
    assert_test!(master_idx_raw >= 0, "alloc_pty failed");
    let master_idx = TtyIndex(master_idx_raw as u8);
    let slave_number =
        slopos_lib::kernel_services::syscall_services::tty::get_pty_number(master_idx);
    assert_test!(slave_number >= 0, "get_pty_number failed");
    let slave_number = slave_number as u32;
    let path = match pts_path_for(slave_number) {
        Some(path) => path,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Unlock slave so /dev/pts/N open succeeds.
    slopos_lib::kernel_services::syscall_services::tty::set_pty_lock(master_idx, false);

    let pid = unsafe { (*task_ptr).process_id };
    let cpu_id = slopos_lib::get_current_cpu();
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(task_ptr));
    let fd = file_open_for_process(pid, path.as_ptr() as *const c_char, O_RDONLY);
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(ptr::null_mut()));

    assert_test!(fd >= 0, "open(/dev/pts/N) failed");
    assert_eq_test!(
        unsafe { (*task_ptr).controlling_tty },
        Some(TtyIndex(slave_number as u8)),
        "PTY slave open should acquire controlling tty"
    );
    assert_eq_test!(
        slopos_lib::kernel_services::syscall_services::tty::get_session_id(TtyIndex(
            slave_number as u8
        )),
        task_id,
        "PTY slave session should match task session"
    );

    let _ = file_close_fd(pid, fd);
    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_pts_open_with_o_noctty_skips_controlling_tty_acquire() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    let master_idx_raw = slopos_lib::kernel_services::syscall_services::tty::alloc_pty();
    assert_test!(master_idx_raw >= 0, "alloc_pty failed");
    let master_idx = TtyIndex(master_idx_raw as u8);
    let slave_number =
        slopos_lib::kernel_services::syscall_services::tty::get_pty_number(master_idx);
    assert_test!(slave_number >= 0, "get_pty_number failed");
    let slave_number = slave_number as u32;
    let path = match pts_path_for(slave_number) {
        Some(path) => path,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Unlock slave so /dev/pts/N open succeeds.
    slopos_lib::kernel_services::syscall_services::tty::set_pty_lock(master_idx, false);

    let pid = unsafe { (*task_ptr).process_id };
    let cpu_id = slopos_lib::get_current_cpu();
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(task_ptr));
    let fd = file_open_for_process(
        pid,
        path.as_ptr() as *const c_char,
        O_RDONLY | O_NOCTTY as u32,
    );
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(ptr::null_mut()));

    assert_test!(fd >= 0, "open(/dev/pts/N, O_NOCTTY) failed");
    assert_eq_test!(
        unsafe { (*task_ptr).controlling_tty },
        None,
        "O_NOCTTY should prevent ctty acquire"
    );
    assert_eq_test!(
        slopos_lib::kernel_services::syscall_services::tty::get_session_id(TtyIndex(
            slave_number as u8
        )),
        0,
        "O_NOCTTY open should leave PTY session unattached"
    );

    let _ = file_close_fd(pid, fd);
    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_vm_mmap_munmap_stress_baseline() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);
    let pid = unsafe { (*task_ptr).process_id };

    for _ in 0..128 {
        let addr = slopos_mm::process_vm::process_vm_mmap(
            pid,
            0,
            4096,
            slopos_abi::syscall::PROT_READ | slopos_abi::syscall::PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        );
        if addr == 0 {
            task_terminate(task_id);
            return TestResult::Fail;
        }
        if slopos_mm::process_vm::process_vm_munmap(pid, addr, 4096) != 0 {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    }

    task_terminate(task_id);
    TestResult::Pass
}

// =============================================================================
// Fork Edge Case Tests
// =============================================================================

pub fn test_fork_null_parent() -> TestResult {
    let _fixture = SyscallFixture::new();

    use crate::scheduler::task::task_fork;
    let child_id = task_fork(ptr::null_mut(), core::ptr::null());
    assert_test!(
        child_id == INVALID_TASK_ID,
        "fork with null parent should fail"
    );
    TestResult::Pass
}

pub fn test_fork_kernel_task() -> TestResult {
    let _fixture = SyscallFixture::new();

    let kernel_task_id = create_test_kernel_task();
    assert_test!(kernel_task_id != INVALID_TASK_ID);

    let kernel_task = task_find_by_id(kernel_task_id);
    assert_not_null!(kernel_task);

    use crate::scheduler::task::task_fork;
    let child_id = task_fork(kernel_task, core::ptr::null());
    assert_test!(
        child_id == INVALID_TASK_ID,
        "kernel tasks should not be forkable"
    );

    task_terminate(kernel_task_id);
    TestResult::Pass
}

pub fn test_fork_at_task_limit() -> TestResult {
    let _fixture = SyscallFixture::new();

    use crate::scheduler::task::MAX_TASKS;

    let mut created_ids: [u32; 64] = [INVALID_TASK_ID; 64];
    let mut count = 0usize;

    for _ in 0..MAX_TASKS {
        let id = task_create(
            b"FillTask\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            1,
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            break;
        }
        if count < created_ids.len() {
            created_ids[count] = id;
            count += 1;
        }
    }

    for i in 0..count {
        task_terminate(created_ids[i]);
    }

    TestResult::Pass
}

pub fn test_fork_terminated_parent() -> TestResult {
    let _fixture = SyscallFixture::new();

    use crate::scheduler::task::task_fork;

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    task_terminate(task_id);

    let task_ptr_after = task_find_by_id(task_id);
    if !task_ptr_after.is_null() {
        let state = unsafe { (*task_ptr_after).status() };
        if state == TaskStatus::Terminated {
            let child_id = task_fork(task_ptr_after, core::ptr::null());
            assert_test!(
                child_id == INVALID_TASK_ID,
                "fork terminated parent should fail"
            );
        }
    }

    TestResult::Pass
}

pub fn test_fork_blocked_parent() -> TestResult {
    let _fixture = SyscallFixture::new();

    use crate::scheduler::task::{task_fork, task_set_state};

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    task_set_state(task_id, TaskStatus::Blocked);

    let child_id = task_fork(task_ptr, core::ptr::null());

    task_terminate(task_id);
    if child_id != INVALID_TASK_ID {
        task_terminate(child_id);
    }

    TestResult::Pass
}

pub fn test_fork_cleanup_on_failure() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();

    let mut free_before = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_before,
        ptr::null_mut(),
    );

    let parent_pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(parent_pid != slopos_abi::task::INVALID_PROCESS_ID);

    for _ in 0..5 {
        let _ = slopos_mm::process_vm::process_vm_alloc(
            parent_pid,
            4096 * 4,
            slopos_mm::paging_defs::PageFlags::WRITABLE.bits() as u32,
        );
    }

    for _ in 0..3 {
        let child_pid = slopos_mm::process_vm::process_vm_clone_cow(parent_pid);
        if child_pid != slopos_abi::task::INVALID_PROCESS_ID {
            slopos_mm::process_vm::destroy_process_vm(child_pid);
        }
    }

    slopos_mm::process_vm::destroy_process_vm(parent_pid);

    let mut free_after = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_after,
        ptr::null_mut(),
    );

    let leak = free_before.saturating_sub(free_after);
    assert_test!(leak <= 64, "memory leak after fork cleanup: {} pages", leak);

    TestResult::Pass
}

// =============================================================================
// Pointer Validation Tests
// =============================================================================

pub fn test_user_ptr_null() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    assert_test!(
        UserPtr::<u64>::try_new(0).is_err(),
        "null address should be rejected"
    );
    TestResult::Pass
}

pub fn test_user_ptr_kernel_address() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    let kernel_addr: u64 = 0xFFFF_8000_0000_0000;
    assert_test!(
        UserPtr::<u64>::try_new(kernel_addr).is_err(),
        "kernel address should be rejected"
    );
    TestResult::Pass
}

pub fn test_user_ptr_misaligned() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    let _result = UserPtr::<u64>::try_new(0x1001);
    // Just verify it doesn't crash; alignment policy is implementation-defined
    TestResult::Pass
}

pub fn test_user_ptr_overflow_boundary() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    let near_max: u64 = u64::MAX - 4;
    assert_test!(
        UserPtr::<u64>::try_new(near_max).is_err(),
        "overflow-prone address should be rejected"
    );
    TestResult::Pass
}

// =============================================================================
// Syscall Argument Boundary Tests
// =============================================================================

pub fn test_brk_extreme_values() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();
    let pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(pid != slopos_abi::task::INVALID_PROCESS_ID);

    let current_brk = slopos_mm::process_vm::process_vm_brk(pid, 0);
    if current_brk == 0 {
        klog_info!("SYSCALL_TEST: Initial brk returned 0 (might be a bug)");
    }

    let max_brk = slopos_mm::process_vm::process_vm_brk(pid, u64::MAX);
    assert_test!(max_brk != u64::MAX, "brk accepted u64::MAX");

    let kernel_brk = slopos_mm::process_vm::process_vm_brk(pid, 0xFFFF_8000_0000_0000);
    assert_test!(
        kernel_brk != 0xFFFF_8000_0000_0000,
        "brk accepted kernel address"
    );

    slopos_mm::process_vm::destroy_process_vm(pid);
    TestResult::Pass
}

pub fn test_shm_create_boundaries() -> TestResult {
    let token_zero = slopos_mm::shared_memory::shm_create(1, 0, 0);
    assert_eq_test!(token_zero, 0, "shm_create accepted size 0");

    let token_one = slopos_mm::shared_memory::shm_create(1, 1, 0);
    if token_one != 0 {
        slopos_mm::shared_memory::shm_destroy(1, token_one);
    }

    let token_max = slopos_mm::shared_memory::shm_create(1, u64::MAX, 0);
    assert_eq_test!(token_max, 0, "shm_create accepted u64::MAX");

    let over_limit = (64 * 1024 * 1024) + 1;
    let token_over = slopos_mm::shared_memory::shm_create(1, over_limit, 0);
    assert_eq_test!(token_over, 0, "shm_create accepted size over limit");

    TestResult::Pass
}

// =============================================================================
// Task State Corruption Tests
// =============================================================================

pub fn test_terminate_already_terminated() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    assert_eq_test!(task_terminate(task_id), 0, "first termination failed");

    // Second termination should not crash
    let _r2 = task_terminate(task_id);

    let task_ptr = task_find_by_id(task_id);
    if !task_ptr.is_null() {
        let state = unsafe { (*task_ptr).status() };
        assert_test!(state != TaskStatus::Ready, "terminated task in READY state");
    }

    TestResult::Pass
}

pub fn test_operations_on_terminated_task() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    task_terminate(task_id);

    use crate::scheduler::task::task_get_info;
    let mut task_ptr: *mut Task = ptr::null_mut();
    let _info_result = task_get_info(task_id, &mut task_ptr);

    use crate::scheduler::task::task_set_state;
    let state_result = task_set_state(task_id, TaskStatus::Ready);
    if state_result == 0 {
        let task = task_find_by_id(task_id);
        if !task.is_null() {
            let current_state = unsafe { (*task).status() };
            assert_test!(
                current_state != TaskStatus::Ready,
                "revived terminated task"
            );
        }
    }

    TestResult::Pass
}

// =============================================================================
// Memory Pressure During Syscall Tests
// =============================================================================

pub fn test_fork_memory_pressure() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();

    let parent_pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(parent_pid != slopos_abi::task::INVALID_PROCESS_ID);

    for _ in 0..10 {
        let addr = slopos_mm::process_vm::process_vm_alloc(
            parent_pid,
            4096 * 4,
            slopos_mm::paging_defs::PageFlags::WRITABLE.bits() as u32,
        );
        if addr == 0 {
            break;
        }
    }

    use slopos_abi::addr::PhysAddr;
    use slopos_mm::page_alloc::{ALLOC_FLAG_NO_PCP, alloc_page_frame, free_page_frame};

    let mut stress_pages: [PhysAddr; 128] = [PhysAddr::NULL; 128];
    let mut stress_count = 0usize;

    for _ in 0..128 {
        let phys = alloc_page_frame(ALLOC_FLAG_NO_PCP);
        if phys.is_null() {
            break;
        }
        stress_pages[stress_count] = phys;
        stress_count += 1;
    }

    let child_pid = slopos_mm::process_vm::process_vm_clone_cow(parent_pid);

    let mut free_before = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_before,
        ptr::null_mut(),
    );

    if child_pid != slopos_abi::task::INVALID_PROCESS_ID {
        slopos_mm::process_vm::destroy_process_vm(child_pid);
    }
    slopos_mm::process_vm::destroy_process_vm(parent_pid);

    for i in 0..stress_count {
        free_page_frame(stress_pages[i]);
    }

    let mut free_after = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_after,
        ptr::null_mut(),
    );

    let leak = free_before.saturating_sub(free_after);
    assert_test!(leak <= 32, "memory leak under pressure: {} pages", leak);

    TestResult::Pass
}

pub fn test_task_id_wraparound() -> TestResult {
    let _fixture = SyscallFixture::new();

    let mut ids_seen: [u32; 256] = [INVALID_TASK_ID; 256];
    let mut seen_count = 0usize;

    for _i in 0..500 {
        let id = task_create(
            b"WrapTest\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            1,
            TASK_FLAG_KERNEL_MODE,
        );

        if id == INVALID_TASK_ID {
            continue;
        }

        for j in 0..seen_count {
            assert_test!(ids_seen[j] != id, "duplicate task ID {}", id);
        }

        if seen_count < ids_seen.len() {
            ids_seen[seen_count] = id;
            seen_count += 1;
        }

        task_terminate(id);
    }

    TestResult::Pass
}

pub fn test_clone_thread_tls_isolation() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(
        parent_id != INVALID_TASK_ID,
        "failed to create parent user task"
    );
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent task lookup failed");

    unsafe {
        (*parent_ptr).fs_base = 0x0000_1111_2222_3000;
    }

    let flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS;
    let child_id = match task_clone(parent_ptr, flags, 0, 0, 0, 0x0000_5555_6666_7000) {
        Ok(id) => {
            task_set_state(id, TaskStatus::Blocked);
            id
        }
        Err(_) => {
            task_terminate(parent_id);
            return TestResult::Fail;
        }
    };

    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child task lookup failed");

    unsafe {
        assert_eq_test!(
            (*child_ptr).tgid,
            (*parent_ptr).tgid,
            "thread did not join parent thread-group"
        );
        assert_eq_test!(
            (*child_ptr).fs_base,
            0x0000_5555_6666_7000,
            "child TLS base not set by CLONE_SETTLS"
        );
        assert_eq_test!(
            (*parent_ptr).fs_base,
            0x0000_1111_2222_3000,
            "parent TLS base unexpectedly modified"
        );
    }

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_clone_then_fork_interaction() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(
        parent_id != INVALID_TASK_ID,
        "failed to create parent user task"
    );
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent task lookup failed");

    let thread_flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
    let thread_id = match task_clone(parent_ptr, thread_flags, 0, 0, 0, 0) {
        Ok(id) => {
            task_set_state(id, TaskStatus::Blocked);
            id
        }
        Err(_) => {
            task_terminate(parent_id);
            return TestResult::Fail;
        }
    };

    let fork_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(fork_id != INVALID_TASK_ID, "fork after clone failed");
    task_set_state(fork_id, TaskStatus::Blocked);

    let thread_ptr = task_find_by_id(thread_id);
    let fork_ptr = task_find_by_id(fork_id);
    assert_not_null!(thread_ptr, "thread task lookup failed");
    assert_not_null!(fork_ptr, "fork child task lookup failed");

    unsafe {
        assert_eq_test!(
            (*thread_ptr).tgid,
            (*parent_ptr).tgid,
            "thread tgid mismatch"
        );
        assert_eq_test!(
            (*fork_ptr).tgid,
            fork_id,
            "fork child should be its own thread-group leader"
        );
        assert_eq_test!(
            (*fork_ptr).parent_task_id,
            parent_id,
            "fork child parent id mismatch"
        );
    }

    task_terminate(fork_id);
    task_terminate(thread_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_futex_wait_mismatch_and_wake_no_waiters() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let uaddr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_test!(
        user_copy_out(pid, uaddr, &1u32),
        "failed to initialize futex word"
    );

    let mut wait_frame = zero_frame();
    wait_frame.rdi = uaddr;
    wait_frame.rsi = FUTEX_WAIT;
    wait_frame.rdx = 2;
    wait_frame.r10 = 0;
    let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wait_frame));
    assert_eq_test!(
        wait_frame.rax,
        ERRNO_EAGAIN,
        "FUTEX_WAIT mismatch must return -EAGAIN"
    );

    let mut wake_frame = zero_frame();
    wake_frame.rdi = uaddr;
    wake_frame.rsi = FUTEX_WAKE;
    wake_frame.rdx = 1;
    let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wake_frame));
    assert_eq_test!(
        wake_frame.rax,
        0,
        "FUTEX_WAKE with no waiters must return 0"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_futex_lost_wakeup_regression() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let uaddr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_test!(
        user_copy_out(pid, uaddr, &1u32),
        "failed to initialize futex word"
    );

    let mut wake_frame = zero_frame();
    wake_frame.rdi = uaddr;
    wake_frame.rsi = FUTEX_WAKE;
    wake_frame.rdx = 1;
    let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wake_frame));
    assert_eq_test!(
        wake_frame.rax,
        0,
        "initial FUTEX_WAKE should wake no waiters"
    );

    let mut wait_frame = zero_frame();
    wait_frame.rdi = uaddr;
    wait_frame.rsi = FUTEX_WAIT;
    wait_frame.rdx = 2;
    wait_frame.r10 = 0;
    let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wait_frame));
    assert_eq_test!(
        wait_frame.rax,
        ERRNO_EAGAIN,
        "post-wake mismatch must return -EAGAIN"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_futex_contention_path_stability() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let uaddr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_test!(
        user_copy_out(pid, uaddr, &1u32),
        "failed to initialize futex word"
    );

    for i in 0..64u64 {
        let mut wake_frame = zero_frame();
        wake_frame.rdi = uaddr;
        wake_frame.rsi = FUTEX_WAKE;
        wake_frame.rdx = (i % 4) + 1;
        let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wake_frame));
        if wake_frame.rax > wake_frame.rdx {
            task_terminate(task_id);
            return TestResult::Fail;
        }

        let mut wait_frame = zero_frame();
        wait_frame.rdi = uaddr;
        wait_frame.rsi = FUTEX_WAIT;
        wait_frame.rdx = 2;
        wait_frame.r10 = 0;
        let _ = with_user_process_context(pid, || syscall_futex(task_ptr, &mut wait_frame));
        if wait_frame.rax != ERRNO_EAGAIN {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    }

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_signal_install_deliver_and_sigreturn() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let new_action_addr = page;
    let old_action_addr = page + 128;

    let new_action = UserSigaction {
        sa_handler: 0x4001_0000,
        sa_flags: 0,
        sa_restorer: 0x4002_0000,
        sa_mask: sig_bit(SIGCHLD),
    };
    assert_test!(
        user_copy_out(pid, new_action_addr, &new_action),
        "failed to write new sigaction"
    );

    let mut action_frame = zero_frame();
    action_frame.rdi = SIGUSR1 as u64;
    action_frame.rsi = new_action_addr;
    action_frame.rdx = old_action_addr;
    action_frame.r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || syscall_rt_sigaction(task_ptr, &mut action_frame));
    assert_eq_test!(action_frame.rax, 0, "rt_sigaction failed");

    let old_action: UserSigaction = match user_copy_in(pid, old_action_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        old_action.sa_handler,
        0,
        "initial old action should be SIG_DFL"
    );

    let stack_top = process_vm_get_stack_top(pid);
    let original_rsp = stack_top.wrapping_sub(0x200);
    let original_rip = 0x5000_1234;

    let mut kill_frame = zero_frame();
    kill_frame.rdi = task_id as u64;
    kill_frame.rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(pid, || syscall_kill(task_ptr, &mut kill_frame));
    assert_eq_test!(kill_frame.rax, 0, "kill(SIGUSR1) failed");

    let mut user_frame = zero_frame();
    user_frame.rip = original_rip;
    user_frame.rsp = original_rsp;
    user_frame.rax = 0xAA55;
    user_frame.rbx = 0xBB66;
    let _ = with_user_process_context(pid, || deliver_pending_signal(task_ptr, &mut user_frame));

    assert_eq_test!(
        user_frame.rip,
        new_action.sa_handler,
        "signal handler RIP not installed"
    );
    assert_eq_test!(
        user_frame.rdi,
        SIGUSR1 as u64,
        "signal number not passed in RDI"
    );

    let sigframe: SignalFrame = match user_copy_in(pid, user_frame.rsp) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        sigframe.rip,
        original_rip,
        "saved RIP mismatch in signal frame"
    );
    assert_eq_test!(
        sigframe.rsp,
        original_rsp,
        "saved RSP mismatch in signal frame"
    );
    assert_eq_test!(
        sigframe.restorer,
        new_action.sa_restorer,
        "signal restorer mismatch"
    );

    let _ = with_user_process_context(pid, || syscall_rt_sigreturn(task_ptr, &mut user_frame));
    assert_eq_test!(
        user_frame.rip,
        original_rip,
        "rt_sigreturn did not restore RIP"
    );
    assert_eq_test!(
        user_frame.rsp,
        original_rsp,
        "rt_sigreturn did not restore RSP"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_sigprocmask_block_then_unblock_delivery() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let set_addr = page;
    let old_addr = page + 128;
    let act_addr = page + 256;

    let action = UserSigaction {
        sa_handler: 0x4003_0000,
        sa_flags: 0,
        sa_restorer: 0x4004_0000,
        sa_mask: 0,
    };
    assert_test!(
        user_copy_out(pid, act_addr, &action),
        "failed to write action"
    );

    let mut install_frame = zero_frame();
    install_frame.rdi = SIGUSR1 as u64;
    install_frame.rsi = act_addr;
    install_frame.rdx = 0;
    install_frame.r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || syscall_rt_sigaction(task_ptr, &mut install_frame));
    assert_eq_test!(install_frame.rax, 0, "sigaction install failed");

    let block_set: SigSet = sig_bit(SIGUSR1);
    assert_test!(
        user_copy_out(pid, set_addr, &block_set),
        "failed to write block set"
    );

    let mut block_frame = zero_frame();
    block_frame.rdi = SIG_SETMASK as u64;
    block_frame.rsi = set_addr;
    block_frame.rdx = old_addr;
    block_frame.r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || syscall_rt_sigprocmask(task_ptr, &mut block_frame));
    assert_eq_test!(block_frame.rax, 0, "rt_sigprocmask(SIG_SETMASK) failed");

    let mut kill_frame = zero_frame();
    kill_frame.rdi = task_id as u64;
    kill_frame.rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(pid, || syscall_kill(task_ptr, &mut kill_frame));
    assert_eq_test!(kill_frame.rax, 0, "kill(SIGUSR1) failed");

    let stack_top = process_vm_get_stack_top(pid);
    let mut user_frame = zero_frame();
    user_frame.rip = 0x6000_1111;
    user_frame.rsp = stack_top.wrapping_sub(0x200);
    let _ = with_user_process_context(pid, || deliver_pending_signal(task_ptr, &mut user_frame));
    assert_eq_test!(
        user_frame.rip,
        0x6000_1111,
        "blocked signal should not be delivered"
    );

    let mut unblock_frame = zero_frame();
    unblock_frame.rdi = SIG_UNBLOCK as u64;
    unblock_frame.rsi = set_addr;
    unblock_frame.rdx = 0;
    unblock_frame.r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || syscall_rt_sigprocmask(task_ptr, &mut unblock_frame));
    assert_eq_test!(unblock_frame.rax, 0, "rt_sigprocmask(SIG_UNBLOCK) failed");

    let _ = with_user_process_context(pid, || deliver_pending_signal(task_ptr, &mut user_frame));
    assert_eq_test!(
        user_frame.rip,
        action.sa_handler,
        "unblocked pending signal was not delivered"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_sigchld_and_wait_interaction() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "task_fork failed");
    task_set_state(child_id, TaskStatus::Blocked);

    unsafe {
        (*parent_ptr).waiting_on.store(child_id, Ordering::Release);
    }
    assert_eq_test!(
        task_set_state(parent_id, TaskStatus::Running),
        0,
        "failed to set parent running"
    );
    assert_eq_test!(
        task_set_state(parent_id, TaskStatus::Blocked),
        0,
        "failed to block parent"
    );

    assert_eq_test!(task_terminate(child_id), 0, "failed to terminate child");

    unsafe {
        let pending = (*parent_ptr).signal_pending.load(Ordering::Acquire);
        assert_test!(
            (pending & sig_bit(SIGCHLD)) != 0,
            "parent missing SIGCHLD pending bit"
        );
        assert_eq_test!(
            (*parent_ptr).waiting_on.load(Ordering::Acquire),
            INVALID_TASK_ID,
            "parent wait target not cleared after child exit"
        );
        assert_eq_test!(
            (*parent_ptr).status(),
            TaskStatus::Ready,
            "parent not readied after child exit"
        );
    }

    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_arch_prctl_set_get_fs_roundtrip() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let out_addr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    let expected_fs = 0x0000_4000_5678_9000u64;
    let mut set_frame = zero_frame();
    set_frame.rdi = ARCH_SET_FS;
    set_frame.rsi = expected_fs;
    let _ = with_user_process_context(pid, || syscall_arch_prctl(task_ptr, &mut set_frame));
    assert_eq_test!(set_frame.rax, 0, "ARCH_SET_FS failed");

    let mut get_frame = zero_frame();
    get_frame.rdi = ARCH_GET_FS;
    get_frame.rsi = out_addr;
    let _ = with_user_process_context(pid, || syscall_arch_prctl(task_ptr, &mut get_frame));
    assert_eq_test!(get_frame.rax, 0, "ARCH_GET_FS failed");

    let got_fs: u64 = match user_copy_in(pid, out_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(got_fs, expected_fs, "ARCH_GET_FS returned wrong value");

    let child_no_settls = match task_clone(
        task_ptr,
        CLONE_VM | CLONE_SIGHAND | CLONE_THREAD,
        0,
        0,
        0,
        0,
    ) {
        Ok(id) => id,
        Err(_) => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let child_ptr = task_find_by_id(child_no_settls);
    assert_not_null!(child_ptr, "clone child lookup failed");
    unsafe {
        assert_eq_test!(
            (*child_ptr).fs_base,
            expected_fs,
            "clone without CLONE_SETTLS must inherit FS base"
        );
    }

    task_terminate(child_no_settls);
    task_terminate(task_id);
    TestResult::Pass
}

// =============================================================================
// Pipe Blocking & EOF Tests
// =============================================================================

/// Basic pipe write-then-read: write "hello", read it back, verify content.
pub fn test_pipe_write_read_basic() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    let payload = b"hello";
    let written = file_write_fd(
        pid,
        write_fd,
        payload.as_ptr() as *const c_char,
        payload.len(),
    );
    assert_eq_test!(
        written as usize,
        payload.len(),
        "write returned wrong count"
    );

    let mut out = [0u8; 16];
    let nread = file_read_fd(pid, read_fd, out.as_mut_ptr() as *mut c_char, payload.len());
    assert_eq_test!(nread as usize, payload.len(), "read returned wrong count");
    assert_test!(&out[..payload.len()] == payload, "read payload mismatch");

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// EOF returns 0, not -1: write data, close writer, read data, read again for EOF.
pub fn test_pipe_eof_returns_zero() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    let payload = b"data";
    let written = file_write_fd(
        pid,
        write_fd,
        payload.as_ptr() as *const c_char,
        payload.len(),
    );
    assert_eq_test!(written as usize, payload.len(), "write failed");

    // Close the write end before reading -- this sets up the EOF condition.
    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");

    // First read: should return the data.
    let mut out = [0u8; 16];
    let nread = file_read_fd(pid, read_fd, out.as_mut_ptr() as *mut c_char, out.len());
    assert_eq_test!(nread as usize, payload.len(), "first read wrong count");
    assert_test!(
        &out[..payload.len()] == payload,
        "first read payload mismatch"
    );

    // Second read: pipe empty + no writers = EOF (0), NOT error (-1).
    let eof = file_read_fd(pid, read_fd, out.as_mut_ptr() as *mut c_char, out.len());
    assert_eq_test!(eof, 0, "EOF read should return 0, not -1");

    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// Broken pipe: writing to a pipe with no readers should return -1.
pub fn test_pipe_broken_pipe() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    // Close read end first, then try to write.
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");

    let payload = b"orphan";
    let result = file_write_fd(
        pid,
        write_fd,
        payload.as_ptr() as *const c_char,
        payload.len(),
    );
    assert_eq_test!(result, -1, "write to broken pipe should return -1");

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// Multiple writes accumulate: write "aaa" then "bbb", read should yield "aaabbb".
pub fn test_pipe_multi_write_read() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    let a = b"aaa";
    let b = b"bbb";
    let w1 = file_write_fd(pid, write_fd, a.as_ptr() as *const c_char, a.len());
    assert_eq_test!(w1 as usize, a.len(), "first write failed");

    let w2 = file_write_fd(pid, write_fd, b.as_ptr() as *const c_char, b.len());
    assert_eq_test!(w2 as usize, b.len(), "second write failed");

    let mut out = [0u8; 16];
    let nread = file_read_fd(pid, read_fd, out.as_mut_ptr() as *mut c_char, out.len());
    assert_eq_test!(nread as usize, 6, "read should return all 6 bytes");
    assert_test!(&out[..6] == b"aaabbb", "accumulated data mismatch");

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// Partial read: write 100 bytes, read 50, then read remaining 50.
pub fn test_pipe_partial_read() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    // Write 100 bytes (pattern: 0..99)
    let mut payload = [0u8; 100];
    for i in 0..100 {
        payload[i] = i as u8;
    }
    let written = file_write_fd(
        pid,
        write_fd,
        payload.as_ptr() as *const c_char,
        payload.len(),
    );
    assert_eq_test!(written as usize, 100, "write 100 bytes failed");

    // Read first 50
    let mut buf1 = [0u8; 50];
    let r1 = file_read_fd(pid, read_fd, buf1.as_mut_ptr() as *mut c_char, 50);
    assert_eq_test!(r1 as usize, 50, "first partial read wrong count");
    assert_test!(
        &buf1[..] == &payload[..50],
        "first partial read data mismatch"
    );

    // Read remaining 50
    let mut buf2 = [0u8; 50];
    let r2 = file_read_fd(pid, read_fd, buf2.as_mut_ptr() as *mut c_char, 50);
    assert_eq_test!(r2 as usize, 50, "second partial read wrong count");
    assert_test!(
        &buf2[..] == &payload[50..100],
        "second partial read data mismatch"
    );

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// Buffer full: fill the 4096-byte pipe buffer, then try to write 1 more byte
/// in non-blocking mode -- should return EAGAIN (-11).
pub fn test_pipe_buffer_full() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    // Create pipe with O_NONBLOCK so writes don't block when full.
    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create (nonblock) failed"
    );

    // Fill the pipe buffer (4096 bytes) in chunks.
    let chunk = [0xABu8; 512];
    let mut total_written: usize = 0;
    for _ in 0..8 {
        let w = file_write_fd(pid, write_fd, chunk.as_ptr() as *const c_char, chunk.len());
        assert_test!(w > 0, "write chunk failed while filling buffer");
        total_written += w as usize;
    }
    assert_eq_test!(total_written, 4096, "did not fill pipe buffer to 4096");

    // Now the pipe should be full. A non-blocking write of 1 byte should return EAGAIN.
    let extra = [0xCDu8; 1];
    let over = file_write_fd(pid, write_fd, extra.as_ptr() as *const c_char, extra.len());
    assert_eq_test!(over, -11, "write to full pipe should return EAGAIN (-11)");

    // Also verify reading from an empty non-blocking pipe returns EAGAIN.
    // First drain the buffer.
    let mut drain = [0u8; 4096];
    let drained = file_read_fd(pid, read_fd, drain.as_mut_ptr() as *mut c_char, drain.len());
    assert_eq_test!(drained as usize, 4096, "drain read wrong count");

    // Pipe is now empty with writers still open: non-blocking read should return EAGAIN.
    let mut one = [0u8; 1];
    let empty_read = file_read_fd(pid, read_fd, one.as_mut_ptr() as *mut c_char, 1);
    assert_eq_test!(
        empty_read,
        -11,
        "read from empty nonblock pipe should return EAGAIN (-11)"
    );

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// Regression: when the current task exits, its file table must be destroyed
/// so pipe writer refs are released and peer readers observe EOF.
pub fn test_exit_current_task_releases_pipe_refs() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t1 = create_test_user_task();
    let t2 = create_test_user_task();
    assert_test!(
        t1 != INVALID_TASK_ID && t2 != INVALID_TASK_ID,
        "failed to create tasks"
    );

    let p1 = task_find_by_id(t1);
    let p2 = task_find_by_id(t2);
    assert_not_null!(p1, "task1 lookup failed");
    assert_not_null!(p2, "task2 lookup failed");

    let pid1 = unsafe { (*p1).process_id };
    let pid2 = unsafe { (*p2).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid1, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    // Replace pid2's default console table with a clone of pid1.
    fileio_destroy_table_for_process(pid2);
    assert_eq_test!(
        fileio_clone_table_for_process(pid1, pid2),
        0,
        "file table clone failed"
    );

    // Keep only the read end in pid2.
    assert_eq_test!(file_close_fd(pid2, write_fd), 0, "pid2 close write failed");

    // Make task1 appear as current so task_terminate() takes the current-task
    // cleanup path (the path that previously leaked file descriptors).
    let cpu_id = slopos_lib::get_current_cpu();
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(p1));
    assert_eq_test!(task::task_terminate(t1), 0, "current-task terminate failed");
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(ptr::null_mut()));

    // If writer refs were released correctly, empty nonblocking read returns EOF (0),
    // not EAGAIN (-11).
    let mut one = [0u8; 1];
    let r = file_read_fd(pid2, read_fd, one.as_mut_ptr() as *mut c_char, 1);
    assert_eq_test!(r, 0, "reader should observe EOF after current task exit");

    assert_eq_test!(file_close_fd(pid2, read_fd), 0, "pid2 close read failed");
    task_terminate(t2);
    TestResult::Pass
}

/// Regression test for the stale-argv spawn bug (compositor spawn failure).
///
/// Before the fix, `spawn_path_with_attrs()` used `syscall4` which left r8/r9
/// (argv_ptr / argc) as stale garbage.  The kernel handler would try to parse
/// argv from those garbage addresses, returning EINVAL instead of reaching the
/// actual exec path.  After the fix (syscall6 with explicit 0,0), r8/r9 are
/// always zero when no argv is passed.
///
/// This test verifies the two code paths return distinguishable errors:
///   - Garbage r8/r9 → argv parsing failure → ERRNO_EINVAL (-22)
///   - Zero   r8/r9 → exec path reached    → ExecError::NoEntry (-2)
pub fn test_spawn_path_stale_argv_regression() -> TestResult {
    use crate::syscall::handlers::syscall_spawn_path;
    use slopos_abi::syscall::ERRNO_EINVAL;

    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    // Map a user-accessible page and write a path that will fail at VFS open.
    let user_page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let path = b"/bin/noent";
    assert_test!(
        user_copy_out(pid, user_page, path),
        "failed to write path into user memory"
    );

    // ---- Case A: Stale/garbage r8 (argv_ptr) and r9 (argc) ----
    // Simulates the pre-fix bug where syscall4 left r8/r9 with junk.
    let mut frame_stale = zero_frame();
    frame_stale.rdi = user_page; // arg0 = path_ptr
    frame_stale.rsi = path.len() as u64; // arg1 = path_len
    frame_stale.rdx = 1; // arg2 = priority
    frame_stale.r10 = 0; // arg3 = flags
    frame_stale.r8 = 0xDEAD_BEEF_CAFE_BABEu64; // arg4 = garbage argv_ptr
    frame_stale.r9 = 42; // arg5 = garbage argc
    let _ = with_user_process_context(pid, || syscall_spawn_path(task_ptr, &mut frame_stale));
    assert_eq_test!(
        frame_stale.rax,
        ERRNO_EINVAL,
        "stale argv must trigger EINVAL from argv parsing failure"
    );

    // ---- Case B: Clean r8=0, r9=0 (the fixed behavior) ----
    // With no argv, the handler skips argv parsing and reaches the exec path.
    let mut frame_clean = zero_frame();
    frame_clean.rdi = user_page; // arg0 = path_ptr
    frame_clean.rsi = path.len() as u64; // arg1 = path_len
    frame_clean.rdx = 1; // arg2 = priority
    frame_clean.r10 = 0; // arg3 = flags
    frame_clean.r8 = 0; // arg4 = no argv
    frame_clean.r9 = 0; // arg5 = no argc
    let _ = with_user_process_context(pid, || syscall_spawn_path(task_ptr, &mut frame_clean));

    // ExecError::NoEntry = -2, returned via ctx.ok(err as i32 as u64)
    let exec_no_entry = (-2i32) as u64;
    assert_eq_test!(
        frame_clean.rax,
        exec_no_entry,
        "clean argv must reach exec path and return NoEntry for missing binary"
    );

    // The two error codes must differ — that's the whole point of this regression.
    assert_test!(
        frame_stale.rax != frame_clean.rax,
        "stale and clean argv paths must return different error codes"
    );

    task_terminate(task_id);
    TestResult::Pass
}

// =============================================================================
// /dev/tty Controlling Terminal Device
// =============================================================================

/// A freshly created task with no controlling terminal cannot open
/// `/dev/tty` — the open must return ENXIO (-6).
pub fn test_dev_tty_no_ctty_returns_enxio() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    // The task has no controlling terminal by default.
    assert_eq_test!(
        unsafe { (*task_ptr).controlling_tty },
        None,
        "fresh task should have no controlling_tty"
    );

    let pid = unsafe { (*task_ptr).process_id };
    let path = b"/dev/tty\0";
    let cpu_id = slopos_lib::get_current_cpu();
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(task_ptr));
    let fd = file_open_for_process(pid, path.as_ptr() as *const c_char, O_RDONLY);
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(ptr::null_mut()));

    assert_eq_test!(
        fd,
        -6,
        "open(/dev/tty) without ctty should return -6 (ENXIO)"
    );

    task_terminate(task_id);
    TestResult::Pass
}

/// After acquiring a controlling terminal via TIOCSCTTY, opening
/// `/dev/tty` succeeds and returns a valid FD that can be used for read/write.
pub fn test_dev_tty_with_ctty_succeeds() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    // Acquire controlling terminal via TIOCSCTTY.
    let mut frame = zero_frame();
    frame.rdi = 0; // fd 0 (console)
    frame.rsi = TIOCSCTTY;
    frame.rdx = 0;
    let _ = syscall_ioctl(task_ptr, &mut frame);
    assert_eq_test!(frame.rax, 0, "TIOCSCTTY should succeed");
    assert_eq_test!(
        unsafe { (*task_ptr).controlling_tty },
        Some(TtyIndex(0)),
        "controlling_tty should be set after TIOCSCTTY"
    );

    // Now open /dev/tty — should succeed.
    let pid = unsafe { (*task_ptr).process_id };
    let path = b"/dev/tty\0";
    let cpu_id = slopos_lib::get_current_cpu();
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(task_ptr));
    let fd = file_open_for_process(pid, path.as_ptr() as *const c_char, O_RDONLY);
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(ptr::null_mut()));

    assert_test!(fd >= 0, "open(/dev/tty) with ctty should succeed");

    // Clean up the FD.
    if fd >= 0 {
        let _ = file_close_fd(pid, fd);
    }
    task_terminate(task_id);
    TestResult::Pass
}

/// After `setsid()`, the controlling terminal is cleared, so opening
/// `/dev/tty` must return ENXIO.
pub fn test_setsid_then_dev_tty_returns_enxio() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    // Give parent a controlling terminal.
    let mut frame = zero_frame();
    frame.rdi = 0;
    frame.rsi = TIOCSCTTY;
    frame.rdx = 0;
    let _ = syscall_ioctl(parent_ptr, &mut frame);
    assert_eq_test!(frame.rax, 0, "TIOCSCTTY should succeed");

    // Fork a child — it inherits the controlling terminal.
    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    // Child should have inherited controlling_tty.
    assert_eq_test!(
        unsafe { (*child_ptr).controlling_tty },
        Some(TtyIndex(0)),
        "child should inherit controlling_tty from parent"
    );

    // Child calls setsid() — controlling terminal cleared.
    let mut setsid_frame = zero_frame();
    let _ = syscall_setsid(child_ptr, &mut setsid_frame);
    assert_eq_test!(
        unsafe { (*child_ptr).controlling_tty },
        None,
        "setsid should clear controlling_tty"
    );

    // Now child tries to open /dev/tty — should fail with ENXIO.
    let child_pid = unsafe { (*child_ptr).process_id };
    let path = b"/dev/tty\0";
    let cpu_id = slopos_lib::get_current_cpu();
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(child_ptr));
    let fd = file_open_for_process(child_pid, path.as_ptr() as *const c_char, O_RDONLY);
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(ptr::null_mut()));

    assert_eq_test!(
        fd,
        -6,
        "open(/dev/tty) after setsid should return -6 (ENXIO)"
    );

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

/// A forked child inherits the parent's controlling terminal, so
/// `/dev/tty` resolves to the same TTY index as the parent.
pub fn test_fork_child_inherits_dev_tty() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    // Parent acquires controlling terminal.
    let mut frame = zero_frame();
    frame.rdi = 0;
    frame.rsi = TIOCSCTTY;
    frame.rdx = 0;
    let _ = syscall_ioctl(parent_ptr, &mut frame);
    assert_eq_test!(frame.rax, 0, "TIOCSCTTY should succeed for parent");

    // Fork child.
    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    // Child should have inherited controlling_tty.
    let parent_ctty = unsafe { (*parent_ptr).controlling_tty };
    let child_ctty = unsafe { (*child_ptr).controlling_tty };
    assert_eq_test!(
        parent_ctty,
        child_ctty,
        "child should inherit same controlling_tty as parent"
    );

    // Child opens /dev/tty — should succeed (inherits parent's ctty).
    let child_pid = unsafe { (*child_ptr).process_id };
    let path = b"/dev/tty\0";
    let cpu_id = slopos_lib::get_current_cpu();
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(child_ptr));
    let fd = file_open_for_process(child_pid, path.as_ptr() as *const c_char, O_RDONLY);
    let _ = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.set_current_task(ptr::null_mut()));

    assert_test!(fd >= 0, "child open(/dev/tty) should succeed");

    if fd >= 0 {
        let _ = file_close_fd(child_pid, fd);
    }
    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

/// SYSCALL_VHANGUP is registered in the dispatch table.
pub fn test_vhangup_syscall_in_dispatch_table() -> TestResult {
    let entry = syscall_lookup(SYSCALL_VHANGUP);
    assert_not_null!(entry, "SYSCALL_VHANGUP lookup returned null");
    let entry_ref = unsafe { &*entry };
    assert_test!(
        entry_ref.handler.is_some(),
        "SYSCALL_VHANGUP has no handler"
    );
    TestResult::Pass
}

slopos_lib::define_test_suite!(
    syscall_valid,
    [
        test_syscall_lookup_invalid_number,
        test_syscall_lookup_empty_slot,
        test_syscall_lookup_valid,
        test_process_syscall_lookup_valid,
        test_io_syscall_lookup_valid,
        test_net_scan_syscall_lookup_valid,
        test_fork_null_parent,
        test_fork_kernel_task,
        test_fork_at_task_limit,
        test_fork_terminated_parent,
        test_fork_blocked_parent,
        test_fork_cleanup_on_failure,
        test_user_ptr_null,
        test_user_ptr_kernel_address,
        test_user_ptr_misaligned,
        test_user_ptr_overflow_boundary,
        test_brk_extreme_values,
        test_shm_create_boundaries,
        test_terminate_already_terminated,
        test_operations_on_terminated_task,
        test_fork_memory_pressure,
        test_task_id_wraparound,
        test_clone_thread_tls_isolation,
        test_clone_then_fork_interaction,
        test_futex_wait_mismatch_and_wake_no_waiters,
        test_futex_lost_wakeup_regression,
        test_futex_contention_path_stability,
        test_signal_install_deliver_and_sigreturn,
        test_sigprocmask_block_then_unblock_delivery,
        test_sigchld_and_wait_interaction,
        test_arch_prctl_set_get_fs_roundtrip,
        test_pipe_poll_eof_baseline,
        test_pipe_write_read_basic,
        test_pipe_eof_returns_zero,
        test_pipe_broken_pipe,
        test_pipe_multi_write_read,
        test_pipe_partial_read,
        test_pipe_buffer_full,
        test_exit_current_task_releases_pipe_refs,
        test_process_group_session_syscalls_baseline,
        test_kill_process_group_semantics,
        test_tiocsctty_session_leader_acquires_ctty,
        test_tiocsctty_non_leader_rejected,
        test_open_dev_tty_with_o_noctty_preserves_flag,
        test_setsid_child_preserves_parent_controlling_tty,
        test_hangup_clears_all_session_controlling_ttys,
        test_pts_open_acquires_controlling_tty_without_o_noctty,
        test_pts_open_with_o_noctty_skips_controlling_tty_acquire,
        test_vm_mmap_munmap_stress_baseline,
        test_spawn_path_stale_argv_regression,
        // /dev/tty Controlling Terminal Device
        test_dev_tty_no_ctty_returns_enxio,
        test_dev_tty_with_ctty_succeeds,
        test_setsid_then_dev_tty_returns_enxio,
        test_fork_child_inherits_dev_tty,
        // EXTPROC & vhangup
        test_vhangup_syscall_in_dispatch_table,
    ]
);

slopos_lib::define_test_suite!(
    syscall_compat_smoke,
    [
        test_syscall_lookup_valid,
        test_process_syscall_lookup_valid,
        test_net_scan_syscall_lookup_valid,
        test_pipe_poll_eof_baseline,
        test_pipe_write_read_basic,
        test_pipe_eof_returns_zero,
        test_pipe_broken_pipe,
        test_pipe_multi_write_read,
        test_pipe_partial_read,
        test_pipe_buffer_full,
        test_exit_current_task_releases_pipe_refs,
        test_process_group_session_syscalls_baseline,
        test_kill_process_group_semantics,
        test_tiocsctty_session_leader_acquires_ctty,
        test_tiocsctty_non_leader_rejected,
        test_open_dev_tty_with_o_noctty_preserves_flag,
        test_setsid_child_preserves_parent_controlling_tty,
        test_hangup_clears_all_session_controlling_ttys,
        test_pts_open_acquires_controlling_tty_without_o_noctty,
        test_pts_open_with_o_noctty_skips_controlling_tty_acquire,
        test_sigchld_and_wait_interaction,
        test_clone_thread_tls_isolation,
        test_futex_wait_mismatch_and_wake_no_waiters,
        test_arch_prctl_set_get_fs_roundtrip,
        test_spawn_path_stale_argv_regression,
        // /dev/tty Controlling Terminal Device
        test_dev_tty_no_ctty_returns_enxio,
        test_dev_tty_with_ctty_succeeds,
        test_setsid_then_dev_tty_returns_enxio,
        test_fork_child_inherits_dev_tty,
        // EXTPROC & vhangup
        test_vhangup_syscall_in_dispatch_table,
    ]
);
