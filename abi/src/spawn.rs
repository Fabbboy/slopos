//! Spawn file-action ABI shared between kernel and userland.
//!
//! A spawn issues a per-fd action list (the `posix_spawn` file-actions model):
//! the child begins with an empty descriptor table and each action installs
//! exactly the descriptors it should inherit. This replaces whole-table
//! inheritance, so a spawner never mutates its own fd table around the call.

/// One file action, tagged by [`SpawnFdActionKind`].
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnFdActionKind {
    /// Share the parent's `src_fd` description into the child's `target_fd`.
    CloneFd = 1,
    /// Move the parent's `src_fd` into the child's `target_fd` (parent slot emptied).
    TransferFd = 2,
    /// Close the child's `target_fd`.
    Close = 3,
    /// Open `open_path` into the child's `target_fd`.
    Open = 4,
}

impl SpawnFdActionKind {
    /// Decode a wire `kind` field.
    #[inline]
    pub const fn from_u32(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::CloneFd),
            2 => Some(Self::TransferFd),
            3 => Some(Self::Close),
            4 => Some(Self::Open),
            _ => None,
        }
    }
}

/// A single spawn file action. Unused fields for a given `kind` are ignored.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SpawnFdAction {
    /// A [`SpawnFdActionKind`] discriminant.
    pub kind: u32,
    /// Source fd in the parent — `CloneFd` / `TransferFd`.
    pub src_fd: i32,
    /// Destination fd in the child — every kind.
    pub target_fd: i32,
    pub _pad: u32,
    /// User pointer to the path bytes — `Open`.
    pub open_path_ptr: u64,
    /// Path length in bytes — `Open`.
    pub open_path_len: u64,
    /// POSIX open flags — `Open`.
    pub open_flags: u32,
    /// Creation mode — `Open`.
    pub open_mode: u32,
}

/// Spawn attributes, passed by pointer in the spawn syscall's `attrs` register.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SpawnAttrs {
    /// Task priority (`TaskPriority::as_u8`).
    pub priority: u8,
    pub _pad: [u8; 3],
    /// Task flags (`TASK_FLAG_*`).
    pub flags: u16,
    pub _pad2: u16,
    /// User pointer to the [`SpawnFdAction`] array.
    pub actions_ptr: u64,
    /// Number of actions.
    pub actions_len: u64,
    /// Signals forced to `SIG_DFL` in the child (POSIX_SPAWN_SETSIGDEF).
    pub sigdefault_mask: u64,
}

/// Upper bound on the action-array length the kernel will read.
pub const SPAWN_MAX_FD_ACTIONS: usize = 64;

const _: () = assert!(core::mem::size_of::<SpawnFdAction>() == 40);
const _: () = assert!(core::mem::align_of::<SpawnFdAction>() == 8);
const _: () = assert!(core::mem::offset_of!(SpawnFdAction, kind) == 0);
const _: () = assert!(core::mem::offset_of!(SpawnFdAction, src_fd) == 4);
const _: () = assert!(core::mem::offset_of!(SpawnFdAction, target_fd) == 8);
const _: () = assert!(core::mem::offset_of!(SpawnFdAction, open_path_ptr) == 16);
const _: () = assert!(core::mem::offset_of!(SpawnFdAction, open_path_len) == 24);
const _: () = assert!(core::mem::offset_of!(SpawnFdAction, open_flags) == 32);
const _: () = assert!(core::mem::offset_of!(SpawnFdAction, open_mode) == 36);

const _: () = assert!(core::mem::size_of::<SpawnAttrs>() == 32);
const _: () = assert!(core::mem::align_of::<SpawnAttrs>() == 8);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, priority) == 0);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, flags) == 4);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, actions_ptr) == 8);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, actions_len) == 16);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, sigdefault_mask) == 24);
