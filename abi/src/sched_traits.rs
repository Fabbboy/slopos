use core::ffi::{c_char, c_int, c_void};

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct FateResult {
    pub token: u32,
    pub value: u32,
}

#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TaskRef(usize);

impl TaskRef {
    pub const NULL: Self = Self(0);

    #[inline]
    pub fn is_null(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub fn from_raw(ptr: *mut c_void) -> Self {
        Self(ptr as usize)
    }

    #[inline]
    pub fn as_raw(self) -> *mut c_void {
        self.0 as *mut c_void
    }
}

impl Default for TaskRef {
    fn default() -> Self {
        Self::NULL
    }
}

pub trait SchedulerServices: Send + Sync {
    fn timer_tick(&self);
    fn handle_post_irq(&self);
    fn request_reschedule_from_interrupt(&self);

    fn get_current_task(&self) -> TaskRef;
    fn yield_cpu(&self);
    fn schedule(&self);
    fn task_terminate(&self, task_id: u32) -> c_int;
    fn block_current_task(&self);
    fn task_is_blocked(&self, task: TaskRef) -> bool;
    fn unblock_task(&self, task: TaskRef) -> c_int;

    fn is_enabled(&self) -> c_int;
    fn is_preemption_enabled(&self) -> c_int;
    fn get_task_stats(&self) -> (u32, u32, u64);
    fn get_scheduler_stats(&self) -> (u64, u64, u32, u32);
    fn register_idle_wakeup_callback(&self, cb: Option<fn() -> c_int>);

    fn fate_spin(&self) -> FateResult;
    fn fate_set_pending(&self, res: FateResult, task_id: u32) -> c_int;
    fn fate_take_pending(&self, task_id: u32) -> Option<FateResult>;
    fn fate_apply_outcome(&self, res: &FateResult, resolution: u32, award: bool);

    fn get_current_task_opaque(&self) -> *mut OpaqueTask;
}

pub trait BootServices: Send + Sync {
    fn is_rsdp_available(&self) -> bool;
    fn get_rsdp_address(&self) -> *const c_void;
    fn gdt_set_kernel_rsp0(&self, rsp0: u64);
    fn is_kernel_initialized(&self) -> bool;
    fn kernel_panic(&self, msg: *const c_char) -> !;
    fn kernel_shutdown(&self, reason: *const c_char) -> !;
    fn kernel_reboot(&self, reason: *const c_char) -> !;
    fn idt_get_gate(&self, vector: u8, entry: *mut c_void) -> c_int;
}

pub trait TaskCleanupHook: Send + Sync {
    fn on_task_terminate(&self, task_id: u32);
}

#[repr(C)]
pub struct OpaqueTask {
    _private: [u8; 0],
}
