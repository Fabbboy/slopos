//! Typed kernel event substrate.
//!
//! [`KernelEvent`] is the single typed vocabulary for waking blocked tasks: each
//! variant names a resource and carries its slot id. The backing wait queues
//! live in the kernel's trusted core; the slot spaces are defined here.

pub const MAX_PIPES: usize = 64;

pub const MAX_TTYS: usize = 32;

pub const MAX_UNIX_SOCKETS: usize = 128;

/// Hash-bucket count for child-exit wait queues. Tasks share buckets; a wakeup
/// landing on an unrelated task is rejected by that task's own exit re-check.
pub const CHILD_EXIT_BUCKETS: usize = 64;

/// Hash-bucket count for signal-pending wait queues. Same collision rationale as
/// [`CHILD_EXIT_BUCKETS`]: a spurious wake re-checks `(pending & mask)`.
pub const SIGNAL_PENDING_BUCKETS: usize = 64;

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

/// Maximum number of open network-state monitors. Each carries its own fixed
/// ring, so this bounds the subsystem's whole memory footprint.
pub const MAX_NETMON: usize = 8;

/// Network-monitor slot, in `0..MAX_NETMON`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NetMonSlot(pub u32);

/// A typed kernel wakeup. The event bus routes each variant with a single
/// match, so adding one without wiring its queue fails to compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelEvent {
    SocketRecv {
        sock: SocketSlot,
    },
    SocketSend {
        sock: SocketSlot,
    },
    SocketAccept {
        sock: SocketSlot,
    },
    PipeRead {
        pipe: PipeSlot,
    },
    PipeWrite {
        pipe: PipeSlot,
    },
    /// Input or a status change became readable on a TTY.
    TtyInput {
        tty: TtySlot,
    },
    /// Output flow control resumed or a status change became writable on a TTY.
    TtyOutput {
        tty: TtySlot,
    },
    /// A readiness change occurred on an AF_UNIX socket (recv and send share
    /// one queue per socket).
    UnixSocket {
        sock: UnixSocketSlot,
    },
    /// A task exited; its waiters (e.g. `waitpid`) should re-check.
    ChildExit {
        task: TaskSlot,
    },
    /// One of this task's children exited. Keyed on the **parent**, unlike
    /// [`KernelEvent::ChildExit`]: a `waitpid(-1)` waiter cannot name one child.
    AnyChildExit {
        parent: TaskSlot,
    },
    /// A signal was raised on a task; a `signalfd` poller re-checks its mask.
    /// Published from every signal-raise site, so signals are in-band events.
    SignalPending {
        task: TaskSlot,
    },
    /// A network-state change was queued into a monitor's ring. Published after
    /// the ring lock is released, so the producer holds nothing when it wakes.
    NetMonitor {
        mon: NetMonSlot,
    },
}
