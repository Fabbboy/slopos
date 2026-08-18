use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use slopos_ostd::lock_class;

use slopos_ostd::klog_info;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, OnceLock, SpinLock};

use super::per_cpu;
use super::scheduler::{
    publish_new_task, run_ready_task_from_idle, set_scheduler_enabled, r#yield,
};
use super::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_SYSTEM, Task, TaskPriority, TaskStatus,
    task_create, task_terminate,
};
use super::work_steal::try_work_steal;
use crate::task_struct::Idle;
use slopos_ostd::task::SchedPlacement;

const MAX_IDLE_CALLBACKS: usize = 4;

struct IdleCallbacks {
    slots: [Option<fn() -> c_int>; MAX_IDLE_CALLBACKS],
    count: usize,
}

impl IdleCallbacks {
    const fn new() -> Self {
        Self {
            slots: [None; MAX_IDLE_CALLBACKS],
            count: 0,
        }
    }
}

static IDLE_CBS: OnceLock<SpinLock<IdleCallbacks>> = OnceLock::new();

pub fn scheduler_register_idle_wakeup_callback(callback: Option<fn() -> c_int>) {
    IDLE_CBS.call_once(|| {
        SpinLock::new(
            IdleCallbacks::new(),
            lock_class!("IDLE_CBS", LOCK_LEVEL_REGISTRY),
        )
    });
    let Some(cb) = callback else { return };
    if let Some(mutex) = IDLE_CBS.get() {
        let mut cbs = mutex.lock();
        let idx = cbs.count;
        if idx < MAX_IDLE_CALLBACKS {
            cbs.slots[idx] = Some(cb);
            cbs.count = idx + 1;
        }
    }
}

use slopos_ostd::task::{KernelThreadEntry, KernelThreadSpawner, SpawnError, SpawnedTaskId};

/// Bridges OSTD's `fn()` entry shape to the scheduler's native
/// `extern "C" fn(*mut c_void)`: the caller's `fn()` rides in the `*mut c_void`
/// payload slot — fn and data pointers share size and alignment on every
/// supported target — and is recovered here at task entry.
pub(crate) extern "C" fn kernel_thread_trampoline(arg: *mut c_void) {
    let raw = arg as *mut ();
    let Some(entry) = slopos_ostd::util::fn_ptr::fn_ptr_decode_opt::<KernelThreadEntry>(raw) else {
        klog_info!("spawn: kernel-thread trampoline received null payload");
        return;
    };

    let fatal_if_panics = slopos_ostd::task::TaskAddr::current().is_some_and(per_cpu::is_idle_task)
        || crate::task_struct::Current::get()
            .is_some_and(|current| current.task().flags & TASK_FLAG_SYSTEM != 0);
    if fatal_if_panics || !slopos_ostd::panic_recovery::production_recovery_enabled() {
        entry();
        return;
    }

    match slopos_ostd::panic_recovery::run_recoverable(entry) {
        Ok(()) => {}
        Err(oops) => {
            klog_info!(
                "panic recovery: kthread task={} {}:{}:{}: {} (oops total={})",
                oops.task_id,
                oops.file.as_str(),
                oops.line,
                oops.column,
                oops.reason.as_str(),
                slopos_ostd::panic_recovery::oops_count(),
            );
        }
    }
}

/// NUL-terminated copy of `name` for the scheduler's C-string name input;
/// truncates at `TASK_NAME_MAX_LEN - 1`.
fn name_to_task_buffer(name: &str) -> [u8; super::task::TASK_NAME_MAX_LEN] {
    let mut buf = [0u8; super::task::TASK_NAME_MAX_LEN];
    let bytes = name.as_bytes();
    let take = core::cmp::min(bytes.len(), super::task::TASK_NAME_MAX_LEN - 1);
    buf[..take].copy_from_slice(&bytes[..take]);
    buf
}

pub struct KernelThreadSpawnerImpl;

impl KernelThreadSpawner for KernelThreadSpawnerImpl {
    fn spawn(
        &self,
        name: &'static str,
        entry: KernelThreadEntry,
        priority: u8,
    ) -> Result<SpawnedTaskId, SpawnError> {
        let payload = (entry as usize) as *mut c_void;
        let name_buf = name_to_task_buffer(name);
        let task_id = task_create(
            name_buf.as_ptr() as *const c_char,
            kernel_thread_trampoline,
            payload,
            priority,
            TASK_FLAG_KERNEL_MODE,
        );
        if task_id == INVALID_TASK_ID {
            return Err(SpawnError::OutOfTaskIds);
        }
        // The registry guard is held across publication so a concurrent
        // terminate cannot invalidate the projection.
        let Some(task) = crate::task::task_find_by_id(task_id) else {
            let _ = task_terminate(task_id);
            return Err(SpawnError::OutOfTaskIds);
        };
        if publish_new_task(&task) != 0 {
            let _ = task_terminate(task_id);
            return Err(SpawnError::ScheduleFailed);
        }
        Ok(SpawnedTaskId::new(task_id))
    }
}

static KERNEL_THREAD_SPAWNER: KernelThreadSpawnerImpl = KernelThreadSpawnerImpl;

/// [`slopos_ostd::task::register_kernel_thread_spawner`] takes a
/// `&'static &'static dyn KernelThreadSpawner`, so the inner reference must
/// itself live in a `static`.
static KERNEL_THREAD_SPAWNER_DYN: &dyn KernelThreadSpawner = &KERNEL_THREAD_SPAWNER;

/// The static reference boot passes to OSTD's
/// `register_kernel_thread_spawner`; stable any time after link.
#[inline]
pub fn kernel_thread_spawner_handle() -> &'static &'static dyn KernelThreadSpawner {
    &KERNEL_THREAD_SPAWNER_DYN
}

/// Production backend for `kernel_io_task::yield_with_deadline`, which defers
/// to a boot-registered impl so the trusted core does not link the scheduler.
fn kernel_io_yield_impl(deadline: slopos_ostd::sync::kernel_io_task::Deadline) {
    use slopos_ostd::sync::kernel_io_task::Deadline;
    match deadline {
        Deadline::Immediate => r#yield(),
        Deadline::AtMs(ms) => {
            super::sleep::block_current_task_with_timeout(ms);
        }
        Deadline::Indefinite => {
            // A far-future deadline rather than none: `Indefinite` is normally
            // driven by a `wait_event` predicate, and a forgotten wake must not
            // wedge the task permanently.
            super::sleep::block_current_task_with_timeout(u32::MAX);
        }
    }
}

/// Stable BSS singleton for the [`YieldBackend`] registration.
static KERNEL_IO_YIELD_BACKEND: slopos_ostd::sync::kernel_io_task::YieldBackend =
    kernel_io_yield_impl;

/// The `YieldBackend` fn pointer boot installs via `register_yield_backend`.
#[inline]
pub fn kernel_io_yield_backend() -> slopos_ostd::sync::kernel_io_task::YieldBackend {
    KERNEL_IO_YIELD_BACKEND
}

/// The idle task's entry point. A CPU normally reaches [`scheduler_loop`]
/// through [`enter_scheduler`]'s stack switch instead; both routes land in the
/// same loop on the same stack, so resuming a seeded idle context does not
/// start a second one.
extern "C" fn idle_task_entry(_: *mut c_void) {
    scheduler_loop(slopos_arch::pcr::get_current_cpu())
}

/// The relaxed half of the bottom half: what needs preemption *enabled*.
///
/// OSTD's bottom-half point runs this after releasing its own preemption guard,
/// which the graveyard requires — its push-side predicate refuses to destroy a
/// task while preemption is disabled.
fn relaxed_bottom_half() -> bool {
    crate::scheduler::drain_deferred_task_reclaim();
    // Here rather than in the guarded phase: a console dump can run for seconds
    // against a 115200-baud UART.
    let console = slopos_ostd::kconsole::drain();
    run_idle_callbacks() || console
}

/// Give OSTD's bottom-half point the work that needs preemption enabled.
pub fn arm_bottom_half(token: &slopos_ostd::sync::BspToken<'_>) {
    slopos_ostd::sync::bh::arm(token, relaxed_bottom_half);
}

/// Poll every registered idle callback, reporting whether any found work.
///
/// Copied out rather than invoked under the lock: the callbacks reach TTY and
/// driver locks, and holding a registry-level lock across them would order the
/// whole driver tree under this one.
fn run_idle_callbacks() -> bool {
    let mut callbacks = [None; MAX_IDLE_CALLBACKS];
    if let Some(mutex) = IDLE_CBS.get() {
        let cbs = mutex.lock();
        callbacks[..cbs.count].copy_from_slice(&cbs.slots[..cbs.count]);
    }

    let mut any_work = false;
    for cb in callbacks.into_iter().flatten() {
        if cb() != 0 {
            any_work = true;
        }
    }
    any_work
}

fn scheduler_loop_bottom_half() -> bool {
    let _window = crate::scheduler::RestoreInterruptState::open_window();

    let mut any_work = run_idle_callbacks();

    slopos_ostd::sync::rcu_process_callbacks();

    // The cheapest place to ack: an idle CPU has no working set to lose.
    slopos_mm::mmu::quiesce::tick();

    // Bounded: each block coalesces under the allocator's cli-lock.
    if slopos_mm::page_alloc::quarantine_has_releasable() {
        slopos_mm::page_alloc::quarantine_release_some(64);
        any_work = true;
    }

    any_work
}

pub fn create_idle_task() -> c_int {
    create_idle_task_for_cpu(0)
}

/// Build an `idle/<cpu>` NUL-terminated name in a stack buffer sized to
/// `TASK_NAME_MAX_LEN`.  Stays no_std and alloc-free; the decimal
/// converter handles up to 10 digits which is far beyond any plausible
/// CPU index.
fn idle_task_name(cpu: usize) -> [u8; super::task::TASK_NAME_MAX_LEN] {
    let mut buf = [0u8; super::task::TASK_NAME_MAX_LEN];
    const PREFIX: &[u8] = b"idle/";
    buf[..PREFIX.len()].copy_from_slice(PREFIX);
    let mut pos = PREFIX.len();

    let mut digits = [0u8; 10];
    let mut len = 0usize;
    let mut n = cpu;
    if n == 0 {
        digits[0] = b'0';
        len = 1;
    } else {
        while n > 0 && len < digits.len() {
            digits[len] = b'0' + (n % 10) as u8;
            n /= 10;
            len += 1;
        }
    }

    let max_digits = (super::task::TASK_NAME_MAX_LEN - 1).saturating_sub(pos);
    let take = len.min(max_digits);
    for i in 0..take {
        buf[pos + i] = digits[len - 1 - i];
    }
    pos += take;
    buf[pos] = 0;
    buf
}

pub fn create_idle_task_for_cpu(cpu_id: usize) -> c_int {
    let name = idle_task_name(cpu_id);
    let idle_task_id = crate::task::task_create(
        name.as_ptr() as *const i8,
        idle_task_entry,
        ptr::null_mut(),
        TaskPriority::Idle.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if idle_task_id == INVALID_TASK_ID {
        return -1;
    }
    let Some(idle_guard) = crate::task::task_find_by_id(idle_task_id) else {
        return -1;
    };

    super::task::task_install_idle_affinity(
        &idle_guard,
        per_cpu::affinity_mask_for_cpu(cpu_id),
        cpu_id as u8,
    );

    // `idle_guard` is the registry guard taken three lines above; both of
    // these are its own state.
    let _ = idle_guard.set_status(TaskStatus::Running);
    idle_guard.set_sched_placement(SchedPlacement::OnCpu);

    super::scheduler::install_idle_task(cpu_id, &idle_guard);

    0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdleStackResolveError {
    MissingIdleTask,
    MissingKernelStack,
}

/// This CPU's idle task and the top of its kernel stack.
///
/// Returns the borrow guard rather than a pointer: the caller is about to
/// switch onto that stack, so it needs the task to still be there when it does,
/// and the guard is what says so. `Idle` carries no lifetime, so the pair can
/// be returned by value.
pub(crate) fn resolve_idle_stack_for_cpu(
    cpu_id: usize,
) -> Result<(Idle, u64), IdleStackResolveError> {
    let Some(idle) = Idle::get(cpu_id) else {
        return Err(IdleStackResolveError::MissingIdleTask);
    };

    let stack_top = idle_stack_top(idle.task())?;
    Ok((idle, stack_top))
}

/// The top of an idle task's kernel stack, or why it cannot be switched onto.
///
/// Split out from [`resolve_idle_stack_for_cpu`] so the "installed but
/// unusable" case is reachable from a test without mutating the live idle task
/// a running CPU is standing on.
pub(crate) fn idle_stack_top(task: &Task) -> Result<u64, IdleStackResolveError> {
    match task.kernel_stack_top {
        0 => Err(IdleStackResolveError::MissingKernelStack),
        top => Ok(top),
    }
}

extern "C" fn scheduler_loop_entry(cpu_id: usize, _idle_task: *mut ()) -> ! {
    scheduler_loop(cpu_id)
}

pub fn enter_scheduler(cpu_id: usize) -> ! {
    // Called exactly once per CPU at boot (BSP from `kernel_main_impl`,
    // APs from `ap_entry_rust`). No re-entry guard — by the "trust internal
    // contracts" convention, re-entry is a caller bug to fix at the call site,
    // not a runtime condition to defend against.
    // (Historical note: a prior `is_enabled()`-keyed guard caused a
    // halt-forever bug because the kernel-test fixture's preconditions
    // tripped the guard; the fixture is now hermetic so the guard's
    // raison d'être disappeared with it.)
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.enable();
    });

    set_scheduler_enabled(true);

    slopos_arch::pcr::mark_cpu_online(cpu_id);
    klog_info!("SCHED: CPU {} scheduler online", cpu_id);

    let (idle_task, idle_stack_top) = match resolve_idle_stack_for_cpu(cpu_id) {
        Ok(values) => values,
        Err(IdleStackResolveError::MissingIdleTask) => {
            klog_info!("SCHED: CPU {} has no idle task, halting", cpu_id);
            slopos_mm::tlb::notify_cpu_offline();
            // Leaving `online` latched on a CPU that parks forever would
            // have the lockup detector watch a CPU that can never tick.
            slopos_arch::pcr::mark_cpu_offline(cpu_id);
            slopos_arch::cpu::disable_interrupts();
            slopos_ostd::cpu::x86_64::core::halt_loop();
        }
        Err(IdleStackResolveError::MissingKernelStack) => {
            klog_info!(
                "SCHED: CPU {} idle task has no kernel stack, halting",
                cpu_id
            );
            slopos_mm::tlb::notify_cpu_offline();
            // Leaving `online` latched on a CPU that parks forever would
            // have the lockup detector watch a CPU that can never tick.
            slopos_arch::pcr::mark_cpu_offline(cpu_id);
            slopos_arch::cpu::disable_interrupts();
            slopos_ostd::cpu::x86_64::core::halt_loop();
        }
    };

    // Hand SafeStack off from the bootstrap stub to the real idle
    // task here — once `dispatch()` stores `PCR.current_task`,
    // every subsequent instrumented prologue on this CPU reads
    // `idle_task.unsafe_stack_sp` via `gs:[CURRENT_TASK]` instead of
    // the per-CPU bootstrap stub's.
    super::scheduler::dispatch(cpu_id, idle_task.task());

    // OSTD's `enter_scheduler_loop_noreturn` folds the one `unsafe`
    // stack-switch primitive behind the documented scheduler-bringup
    // discharge.
    //
    // The payload is null: `scheduler_loop_entry` ignores it and
    // `scheduler_loop` re-reads the idle slot per iteration, so passing the
    // task here would launder a type-erased task handle across a stack switch
    // for a value nothing reads.
    slopos_ostd::cpu::x86_64::stack::enter_scheduler_loop_noreturn(
        idle_stack_top,
        scheduler_loop_entry,
        cpu_id,
        ptr::null_mut(),
    )
}

/// Callback registered by the boot layer to start the per-CPU LAPIC timer.
/// Returns `true` when the timer was successfully started (or already running).
/// Called from the scheduler loop on each AP until it returns `true`.
static AP_TIMER_START_CB: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Register a callback that APs invoke from their scheduler loop to start
/// their LAPIC timer.  The boot layer calls this after calibration so that
/// APs (which may already be running) can pick up the calibrated frequency.
///
/// The callback signature is `fn() -> bool` (returns true once started).
pub fn register_ap_timer_start(cb: fn() -> bool) {
    AP_TIMER_START_CB.store(cb as *mut (), core::sync::atomic::Ordering::Release);
}

/// Try to start the per-CPU LAPIC timer via the registered callback.
/// Called once per scheduler-loop iteration until the timer is running.
fn deferred_start_ap_timer(cpu_id: usize) {
    use core::sync::atomic::{AtomicBool, Ordering};

    static AP_TIMER_DONE: [AtomicBool; slopos_arch::MAX_CPUS] = {
        const FALSE: AtomicBool = AtomicBool::new(false);
        [FALSE; slopos_arch::MAX_CPUS]
    };

    if cpu_id >= AP_TIMER_DONE.len() {
        return;
    }
    if AP_TIMER_DONE[cpu_id].load(Ordering::Relaxed) {
        return;
    }

    let ptr = AP_TIMER_START_CB.load(Ordering::Acquire);
    let Some(cb) = slopos_ostd::util::fn_ptr::fn_ptr_decode_opt::<fn() -> bool>(ptr) else {
        return;
    };
    if cb() {
        klog_info!("SCHED: CPU {} LAPIC timer started (deferred)", cpu_id);
        AP_TIMER_DONE[cpu_id].store(true, Ordering::Relaxed);
    }
}

fn scheduler_loop(cpu_id: usize) -> ! {
    loop {
        // Start the LAPIC timer on this AP once the boot layer registers
        // the callback (after calibration).  No-op after the first success.
        deferred_start_ap_timer(cpu_id);

        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.drain_remote_inbox();
        });

        if per_cpu::should_pause_scheduler_loop(cpu_id) {
            // Deliberately no reclaim and no bottom half: the pause exists so
            // its holder can act against quiescent APs, and both would take
            // allocator and registry locks on this CPU's behalf. Whatever this
            // CPU is holding drains on the iteration after the pause lifts.
            slopos_ostd::sync::rcu_note_qs();
            crate::scheduler::arm_tickless_idle_if_due();
            slopos_ostd::sync::rcu_note_cpu_idle_enter();
            slopos_ostd::cpu::x86_64::core::sti_hlt_cli_atomic();
            slopos_ostd::sync::rcu_note_cpu_idle_exit();
            continue;
        }

        // IRQs must be OFF across the dispatch: `execute_task` →
        // `switch_context` swaps the per-task preempt_count with the PCR
        // and switches registers — an IRQ landing inside that window (and
        // its trap-exit handoff re-entering `schedule()`) interleaves a
        // second switch with the half-completed swap, corrupting the
        // count for the incoming task. `schedule_internal` already
        // brackets its switches with `save_flags_cli`; this idle-loop
        // dispatch was the one unbracketed path (and the one every fresh
        // task's first run goes through). When the switch happens, idle's
        // context saves IF=0 and the flags restore below runs at idle
        // resumption.
        let irq_flags = slopos_arch::cpu::save_flags_cli();
        // Minted per iteration rather than once outside the loop: the slot is
        // the authority for which task this loop dispatches from, and the
        // payload that crossed `enter_scheduler_loop_noreturn`'s stack switch
        // is type-erased and deliberately ignored. Re-reading it here is two
        // atomic loads and removes the question of whether a cached pointer is
        // still the CPU's idle task.
        let Some(idle) = Idle::current() else {
            slopos_arch::cpu::restore_flags(irq_flags);
            slopos_ostd::sync::rcu_note_qs();
            crate::scheduler::arm_tickless_idle_if_due();
            slopos_ostd::sync::rcu_note_cpu_idle_enter();
            slopos_ostd::cpu::x86_64::core::sti_hlt_cli_atomic();
            slopos_ostd::sync::rcu_note_cpu_idle_exit();
            continue;
        };
        let dispatched = run_ready_task_from_idle(cpu_id, idle.task());
        // Drain before re-enabling interrupts: the drain clears the CPU-local
        // previous-task slot while interrupts are disabled, closing the window
        // in which a re-entrant dispatch would park a second reference into the
        // still-occupied slot. The reference is then dropped in the drain's own
        // interrupt window.
        let _ = crate::scheduler::drain_previous_task();
        slopos_arch::cpu::restore_flags(irq_flags);
        crate::scheduler::drain_deferred_task_reclaim();
        if dispatched {
            continue;
        }

        if !per_cpu::are_aps_paused() && try_work_steal() {
            continue;
        }

        // Belt-and-braces: re-enqueue any task stranded READY with no
        // runqueue entry (a lost-enqueue race would otherwise freeze it
        // forever — see `rescue_stranded_ready_tasks`). Runs only when this
        // CPU is fully idle (nothing to run, nothing to steal), so the
        // registry walk costs idle time only.
        crate::scheduler::rescue_stranded_ready_tasks();

        // Before the bottom half, and unconditionally: reaching here proves
        // this CPU holds no read-side section, and the bottom half can loop for
        // as long as it keeps finding work. A report placed after it would be
        // skipped exactly while this CPU is busiest reclaiming — stalling the
        // grace period that the reclamation is itself waiting on.
        slopos_ostd::sync::rcu_note_qs();

        // Nothing to run, nothing to steal: the deferred work runs here rather
        // than at the top of the loop, where it would cost a lock acquisition
        // and a TTY slot walk on every dispatch.
        if scheduler_loop_bottom_half() {
            continue;
        }

        // Tickless-idle: arm one-shot LAPIC for the soonest pending
        // sleep-queue deadline if it falls inside the next periodic
        // tick. See `sched::scheduler::arm_tickless_idle_if_due`.
        crate::scheduler::arm_tickless_idle_if_due();

        slopos_ostd::sync::rcu_note_cpu_idle_enter();
        slopos_ostd::cpu::x86_64::core::sti_hlt_cli_atomic();
        slopos_ostd::sync::rcu_note_cpu_idle_exit();
    }
}
