use alloc::sync::Arc;
use slopos_abi::task::{BlockReason, INVALID_TASK_ID, Task, TaskStatus};
use slopos_lib::{IrqRwLock, IrqRwLockReadGuard, IrqRwLockWriteGuard};

pub type TaskRef = Arc<TaskLock>;
pub type TaskLock = IrqRwLock<Task>;
pub type TaskReadGuard<'a> = IrqRwLockReadGuard<'a, Task>;
pub type TaskWriteGuard<'a> = IrqRwLockWriteGuard<'a, Task>;

pub struct TaskHandle<'a> {
    task: &'a mut Task,
}

impl<'a> TaskHandle<'a> {
    pub fn new(task: &'a mut Task) -> Option<Self> {
        if task.task_id == INVALID_TASK_ID {
            return None;
        }
        Some(Self { task })
    }

    #[inline]
    pub fn id(&self) -> u32 {
        self.task.task_id
    }

    #[inline]
    pub fn status(&self) -> TaskStatus {
        self.task.status()
    }

    #[inline]
    pub fn block_reason(&self) -> BlockReason {
        self.task.block_reason
    }

    #[inline]
    pub fn mark_ready(&mut self) -> bool {
        self.task.mark_ready()
    }

    #[inline]
    pub fn mark_running(&mut self) -> bool {
        self.task.mark_running()
    }

    #[inline]
    pub fn block(&mut self, reason: BlockReason) -> bool {
        self.task.block(reason)
    }

    #[inline]
    pub fn terminate(&mut self) -> bool {
        self.task.terminate()
    }

    #[inline]
    pub fn as_ptr(&self) -> *const Task {
        self.task as *const Task
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut Task {
        self.task as *mut Task
    }

    #[inline]
    pub fn inner(&self) -> &Task {
        self.task
    }

    #[inline]
    pub fn inner_mut(&mut self) -> &mut Task {
        self.task
    }
}
