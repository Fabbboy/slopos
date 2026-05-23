//! Typed kernel event substrate.
//!
//! [`KernelEvent`] is the single typed vocabulary the kernel uses to wake
//! blocked tasks. Each variant names a resource and carries that resource's
//! slot id, so a producer and a consumer that disagree about which resource a
//! wakeup targets become a compile error rather than a silent runtime drift.
//!
//! The backing wait queues live in the kernel's trusted core (the event bus);
//! the slot ids and resource capacities are defined here because this crate is
//! the single source of truth for the kernel's slot spaces.

/// Maximum number of concurrent kernel pipes.
pub const MAX_PIPES: usize = 64;

/// Maximum number of TTY instances.
pub const MAX_TTYS: usize = 32;

/// Maximum number of concurrent AF_UNIX sockets.
pub const MAX_UNIX_SOCKETS: usize = 32;

/// Hash-bucket count for child-exit wait queues. Task ids exceed this count,
/// so several tasks share a bucket; a wakeup that lands on an unrelated task
/// is rejected by that task's own exit re-check, exactly as a hash collision
/// is benign for a keyed futex.
pub const CHILD_EXIT_BUCKETS: usize = 64;

/// Network-socket table slot (TCP / UDP / ICMP), in `0..MAX_SOCKETS`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SocketSlot(pub u32);

/// Kernel pipe slot, in `0..MAX_PIPES`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PipeSlot(pub u32);

/// TTY instance slot, in `0..MAX_TTYS`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TtySlot(pub u32);

/// AF_UNIX socket slot, in `0..MAX_UNIX_SOCKETS`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UnixSocketSlot(pub u32);

/// Task id used as the child-exit wake key.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TaskSlot(pub u32);

/// A typed kernel wakeup.
///
/// Every blocking/waking interaction in the kernel names the resource it
/// targets through one of these variants. The variant set is intentionally
/// exhaustive: the event bus routes each one to a backing queue with a single
/// match, so introducing a resource without wiring its queue fails to compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelEvent {
    /// Data became available to receive on a network socket.
    SocketRecv { sock: SocketSlot },
    /// Send-buffer space became available on a network socket.
    SocketSend { sock: SocketSlot },
    /// An inbound connection landed on a listening socket.
    SocketAccept { sock: SocketSlot },
    /// Data became available to read on a pipe.
    PipeRead { pipe: PipeSlot },
    /// Buffer space became available to write on a pipe.
    PipeWrite { pipe: PipeSlot },
    /// Input or a status change became readable on a TTY.
    TtyInput { tty: TtySlot },
    /// Output flow control resumed or a status change became writable on a TTY.
    TtyOutput { tty: TtySlot },
    /// A readiness change occurred on an AF_UNIX socket (recv and send share
    /// one queue per socket).
    UnixSocket { sock: UnixSocketSlot },
    /// A task exited; its waiters (e.g. `waitpid`) should re-check.
    ChildExit { task: TaskSlot },
}
