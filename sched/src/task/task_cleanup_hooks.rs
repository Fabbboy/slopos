use slopos_ostd::klog_info;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

const MAX_TASK_RESOURCE_HOOKS: usize = 8;

struct TaskResourceCleanupHooks {
    hooks: [Option<fn(u32)>; MAX_TASK_RESOURCE_HOOKS],
    count: usize,
}

impl TaskResourceCleanupHooks {
    const fn new() -> Self {
        Self {
            hooks: [None; MAX_TASK_RESOURCE_HOOKS],
            count: 0,
        }
    }
}

static TASK_RESOURCE_HOOKS: SpinLock<TaskResourceCleanupHooks> = SpinLock::new(
    TaskResourceCleanupHooks::new(),
    lock_class!("TASK_RESOURCE_HOOKS", LOCK_LEVEL_REGISTRY),
);

/// Register a cleanup hook, run both at task termination and at `exec()`
/// before the new process image starts. Append-only; register during
/// subsystem init.
pub fn register_task_resource_cleanup_hook(hook: fn(u32)) {
    let mut reg = TASK_RESOURCE_HOOKS.lock();
    if reg.count >= MAX_TASK_RESOURCE_HOOKS {
        klog_info!("task_resource_hooks: registry full, hook not registered");
        return;
    }
    let idx = reg.count;
    reg.hooks[idx] = Some(hook);
    reg.count += 1;
}

pub(super) fn run_task_resource_cleanup_hooks(task_id: u32) {
    let hooks = TASK_RESOURCE_HOOKS.lock();
    for hook in hooks.hooks.iter().take(hooks.count) {
        if let Some(f) = hook {
            f(task_id);
        }
    }
}

/// Release task-bound resources before exec() replaces the process image.
pub fn task_cleanup_for_exec(task_id: u32) {
    run_task_resource_cleanup_hooks(task_id);
}
