use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use slopos_ostd::klog_info;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, OnceLock, SpinLock};

use super::per_cpu;
use super::scheduler::{
    run_ready_task_from_idle, schedule_new_task, set_scheduler_enabled, r#yield,
};
use super::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, Task, TaskPriority, reap_zombies, task_create,
    task_get_info,
};
use super::work_steal::try_work_steal;

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
    IDLE_CBS.call_once(|| SpinLock::new(IdleCallbacks::new(), LOCK_LEVEL_REGISTRY));
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

// ---------------------------------------------------------------------------
// Kernel-thread spawner: out-of-OSTD impl of `slopos_ostd::task::KernelThreadSpawner`.
// ---------------------------------------------------------------------------
//
// Drivers and other Phase-2 services spawn kernel threads through OSTD's
// safe `slopos_ostd::task::spawn(name, entry, priority)` facade; the
// facade defers to the impl registered here at boot.
//
// The `fn()` entry shape comes from OSTD. Internally we trampoline
// through an `extern "C" fn(*mut c_void)` because that is the
// scheduler's native `TaskEntry` (`*mut c_void` arg is the legacy task
// payload; new spawners stash the fn pointer in the payload slot and
// the trampoline dispatches into it). This trick keeps the scheduler
// internals stable while exposing a modern signature to drivers.

use slopos_ostd::task::{KernelThreadEntry, KernelThreadSpawner, SpawnError, SpawnedTaskId};

/// Trampoline. The OSTD-facing `spawn` API takes a `fn()`; the
/// scheduler's native `TaskEntry` is `extern "C" fn(*mut c_void)` with
/// a different ABI. We bridge by packing the caller's `fn()` into the
/// `*mut c_void` payload slot — `fn` pointers and data pointers share
/// size and alignment on every supported target — and recovering the
/// function pointer here at task entry via OSTD's safe-Rust
/// `fn_ptr_decode_opt` helper.
extern "C" fn kernel_thread_trampoline(arg: *mut c_void) {
    let raw = arg as *mut ();
    let Some(entry) = slopos_ostd::util::fn_ptr::fn_ptr_decode_opt::<KernelThreadEntry>(raw) else {
        klog_info!("spawn: kernel-thread trampoline received null payload");
        return;
    };
    entry();
}

/// Build a NUL-terminated `&str -> [u8; N]` copy for the scheduler's
/// legacy C-string name input. Truncates at `TASK_NAME_MAX_LEN - 1`.
fn name_to_task_buffer(name: &str) -> [u8; super::task::TASK_NAME_MAX_LEN] {
    let mut buf = [0u8; super::task::TASK_NAME_MAX_LEN];
    let bytes = name.as_bytes();
    let take = core::cmp::min(bytes.len(), super::task::TASK_NAME_MAX_LEN - 1);
    buf[..take].copy_from_slice(&bytes[..take]);
    // tail already zero from initialisation
    buf
}

/// Concrete spawner impl. Routes through the existing
/// `task_create` + `schedule_new_task` pair, but uses typed errors
/// instead of the legacy `INVALID_TASK_ID` sentinel.
pub struct KernelThreadSpawnerImpl;

impl KernelThreadSpawner for KernelThreadSpawnerImpl {
    fn spawn(
        &self,
        name: &'static str,
        entry: KernelThreadEntry,
        priority: u8,
    ) -> Result<SpawnedTaskId, SpawnError> {
        // Pack the `fn()` into the scheduler's legacy `*mut c_void`
        // payload slot; `kernel_thread_trampoline` unpacks at task
        // entry. `fn as usize as *mut c_void` is safe Rust on every
        // supported target — function pointers and data pointers have
        // the same layout.
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
        let mut task_ptr: *mut Task = core::ptr::null_mut();
        if task_get_info(task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
            return Err(SpawnError::OutOfTaskIds);
        }
        if schedule_new_task(task_ptr) != 0 {
            return Err(SpawnError::ScheduleFailed);
        }
        Ok(SpawnedTaskId::new(task_id))
    }
}

/// Stable BSS singleton; address is exported via
/// [`kernel_thread_spawner_handle`] for the boot wiring.
static KERNEL_THREAD_SPAWNER: KernelThreadSpawnerImpl = KernelThreadSpawnerImpl;

/// Doubly-indirect reference for
/// [`slopos_ostd::task::register_kernel_thread_spawner`]; the setter
/// requires a `&'static &'static dyn KernelThreadSpawner` so the inner
/// reference must live in a `static`. Mirrors the
/// `BUDDY_ALLOCATOR_DYN` shape in `mm/src/page_alloc/mod.rs`.
static KERNEL_THREAD_SPAWNER_DYN: &dyn KernelThreadSpawner = &KERNEL_THREAD_SPAWNER;

/// Hand boot the static reference it needs to pass to OSTD's
/// `register_kernel_thread_spawner`. Stable address; safe to call any
/// time after link.
#[inline]
pub fn kernel_thread_spawner_handle() -> &'static &'static dyn KernelThreadSpawner {
    &KERNEL_THREAD_SPAWNER_DYN
}

extern "C" fn unified_idle_loop(_: *mut c_void) {
    loop {
        let mut any_work = false;
        if let Some(mutex) = IDLE_CBS.get() {
            let cbs = mutex.lock();
            for slot in &cbs.slots[..cbs.count] {
                if let Some(cb) = slot {
                    if cb() != 0 {
                        any_work = true;
                    }
                }
            }
        }
        if any_work {
            r#yield();
            continue;
        }
        let cpu_id = slopos_arch::pcr::get_current_cpu();
        // idle_time is now incremented per-tick in scheduler_timer_tick(),
        // not per-idle-loop-iteration, so the counter stays in lockstep
        // with total_ticks.
        if cpu_id == 0 {
            slopos_ostd::sync::rcu_process_callbacks();
        }
        slopos_ostd::sync::rcu_note_qs();
        slopos_ostd::cpu::x86_64::core::sti_hlt_cli_atomic();
    }
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
        unified_idle_loop,
        ptr::null_mut(),
        TaskPriority::Idle.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if idle_task_id == INVALID_TASK_ID {
        return -1;
    }
    let mut idle_task: *mut Task = ptr::null_mut();
    if task_get_info(idle_task_id, &mut idle_task) != 0 {
        return -1;
    }

    super::task::task_install_idle_affinity(
        idle_task,
        per_cpu::affinity_mask_for_cpu(cpu_id),
        cpu_id as u8,
    );

    super::scheduler::install_idle_task(cpu_id, idle_task);

    0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdleStackResolveError {
    MissingIdleTask,
    MissingKernelStack,
}

pub(crate) fn resolve_idle_stack_for_cpu(
    cpu_id: usize,
) -> Result<(*mut Task, u64), IdleStackResolveError> {
    let idle_task = super::scheduler::scheduler_get_idle_task_for(cpu_id);
    if idle_task.is_null() {
        return Err(IdleStackResolveError::MissingIdleTask);
    }

    let stack_top = super::task::task_kernel_stack_top(idle_task).unwrap_or(0);
    if stack_top == 0 {
        return Err(IdleStackResolveError::MissingKernelStack);
    }

    Ok((idle_task, stack_top))
}

extern "C" fn scheduler_loop_entry(cpu_id: usize, idle_task: *mut ()) -> ! {
    scheduler_loop(cpu_id, idle_task as *mut Task)
}

pub fn enter_scheduler(cpu_id: usize) -> ! {
    // Called exactly once per CPU at boot (BSP from `kernel_main_impl`,
    // APs from `ap_entry_rust`). No re-entry guard: per CLAUDE.md
    // "trust internal contracts" — re-entry is a caller bug to fix at
    // the call site, not a runtime condition to defend against.
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
            slopos_arch::cpu::disable_interrupts();
            slopos_ostd::cpu::x86_64::core::halt_loop();
        }
        Err(IdleStackResolveError::MissingKernelStack) => {
            klog_info!(
                "SCHED: CPU {} idle task has no kernel stack, halting",
                cpu_id
            );
            slopos_mm::tlb::notify_cpu_offline();
            slopos_arch::cpu::disable_interrupts();
            slopos_ostd::cpu::x86_64::core::halt_loop();
        }
    };

    let return_ctx = per_cpu::get_ap_return_context(cpu_id);
    if !return_ctx.is_null() {
        slopos_ostd::task::switch::init_current_context(return_ctx);
    }

    // Hand SafeStack off from the bootstrap stub to the real idle
    // task here — once `dispatch()` stores `PCR.current_task`,
    // every subsequent instrumented prologue on this CPU reads
    // `idle_task.unsafe_stack_sp` via `gs:[CURRENT_TASK]` instead of
    // the per-CPU bootstrap stub's.
    super::scheduler::dispatch(cpu_id, idle_task);

    // OSTD's `enter_scheduler_loop_noreturn` folds the one `unsafe`
    // stack-switch primitive behind the documented scheduler-bringup
    // discharge.
    slopos_ostd::cpu::x86_64::stack::enter_scheduler_loop_noreturn(
        idle_stack_top,
        scheduler_loop_entry,
        cpu_id,
        idle_task as *mut (),
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

// ---------------------------------------------------------------------------
// NMI watchdog: cross-CPU deadlock detection
// ---------------------------------------------------------------------------

/// Per-CPU watchdog threshold in timer ticks. 50 ticks at 100Hz = 500ms.
/// The global timer counter is incremented by ALL CPUs, so the actual
/// threshold used is `WATCHDOG_PER_CPU_THRESHOLD * num_online_cpus`.
const WATCHDOG_PER_CPU_THRESHOLD: u64 = 50;

/// Grace period after boot before the watchdog activates.  The global
/// counter must reach this value before any checks run, giving all CPUs
/// time to start their LAPIC timers and begin ticking.
const WATCHDOG_WARMUP_TICKS: u64 = 200;

/// Each CPU monitors the next CPU in round-robin order.  If the target has
/// not recorded a timer tick within the scaled threshold, it is presumed
/// stuck with interrupts disabled (deadlocked spinlock) and receives an NMI.
fn check_watchdog_for_neighbor(my_cpu: usize) {
    let num_cpus = slopos_arch::pcr::get_online_cpu_count();
    if num_cpus < 2 {
        return;
    }

    let current_tick = slopos_kernel_services::clock::get_timer_ticks();

    // Don't arm the watchdog during early boot while CPUs are still
    // starting their LAPIC timers.
    if current_tick < WATCHDOG_WARMUP_TICKS {
        return;
    }

    // Find the next online CPU to monitor.
    let target = (my_cpu + 1) % num_cpus;
    if target == my_cpu {
        return;
    }
    if !slopos_arch::pcr::is_cpu_online(target) {
        return;
    }

    let target_tick = super::scheduler::watchdog_last_tick(target);

    // Don't trigger if the target hasn't started ticking yet.
    if target_tick == 0 {
        return;
    }

    // Scale threshold by number of online CPUs since the global tick
    // counter is incremented by every CPU's LAPIC timer.
    let threshold = WATCHDOG_PER_CPU_THRESHOLD * num_cpus as u64;

    if current_tick.saturating_sub(target_tick) > threshold {
        // Target CPU hasn't had a timer tick in >500 ms -- likely stuck.
        if let Some(apic_id) = slopos_arch::pcr::apic_id_from_cpu_index(target) {
            slopos_arch::pcr::send_nmi_to_cpu(apic_id);
        }
    }
}

fn scheduler_loop(cpu_id: usize, idle_task: *mut Task) -> ! {
    loop {
        // Start the LAPIC timer on this AP once the boot layer registers
        // the callback (after calibration).  No-op after the first success.
        deferred_start_ap_timer(cpu_id);

        // NMI watchdog: each CPU monitors the next one in round-robin.
        check_watchdog_for_neighbor(cpu_id);

        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.drain_remote_inbox();
        });

        if per_cpu::should_pause_scheduler_loop(cpu_id) {
            slopos_ostd::sync::rcu_note_qs();
            slopos_ostd::cpu::x86_64::core::sti_hlt_cli_atomic();
            continue;
        }

        if run_ready_task_from_idle(cpu_id, idle_task) {
            continue;
        }

        if !per_cpu::are_aps_paused() && try_work_steal() {
            continue;
        }

        reap_zombies();

        // idle_time is now incremented per-tick in scheduler_timer_tick(),
        // not per-idle-loop-iteration, keeping it in lockstep with total_ticks.
        slopos_ostd::sync::rcu_note_qs();

        slopos_ostd::cpu::x86_64::core::sti_hlt_cli_atomic();
    }
}
