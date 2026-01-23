use alloc::sync::Arc;
use slopos_abi::task::Task;
use slopos_lib::{IrqRwLock, IrqRwLockReadGuard, IrqRwLockWriteGuard};

pub type TaskRef = Arc<TaskLock>;
pub type TaskLock = IrqRwLock<Task>;
pub type TaskReadGuard<'a> = IrqRwLockReadGuard<'a, Task>;
pub type TaskWriteGuard<'a> = IrqRwLockWriteGuard<'a, Task>;
