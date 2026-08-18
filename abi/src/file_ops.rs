//! File-operations vtable for polymorphic file descriptors.

use crate::fs::UserFsStat;
use crate::io::{IoBufRead, IoBufWrite};

/// Result of [`FileOps::poll_fused`]: readiness bits plus registration status.
/// Doing both in one call closes the race between separate register and check.
#[derive(Debug, Clone, Copy)]
pub struct FusedPollResult {
    /// POLL* bitmask of currently ready events.
    pub revents: u16,
    /// `true` if the caller was registered on a wait queue for wakeup.
    pub registered: bool,
    /// Opaque token identifying the open file at registration time. It carries a
    /// generation, so cleanup detects a recycled fd slot instead of retargeting.
    pub open_file_token: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileKind {
    Regular = 0,
    Socket = 1,
    PipeRead = 2,
    PipeWrite = 3,
    Tty = 4,
    Memfd = 5,
    /// SlopRing submission/completion ring (SLOPRING § 3). The fd's
    /// `handle` resolves a ring object in the per-process ring registry.
    Ring = 6,
    /// Process-exit fd (`pidfd_open(2)`). The fd's `handle` is a target task id;
    /// it becomes `POLLIN`-ready once that task exits. Read/write give `-EINVAL`.
    Pidfd = 7,
    /// Signal fd (`signalfd(2)`). `POLLIN`-ready when a signal in its subscribed
    /// mask is pending; `read` drains one `SignalfdSiginfo`.
    Signalfd = 8,
    /// Network-state monitor (`net_monitor`). `POLLIN`-ready when the stack's
    /// configuration changes; `read` drains whole `NetEvent` records.
    Netmon = 9,
}

// `trait FileBacking` lives in `slopos_ostd::process::quota`: its `Charged`
// supertrait needs a feature gate this userland-visible crate may not name.

/// Per-resource-type operations for open file descriptions.
///
/// Implementations are zero-sized; per-open state is named by the opaque
/// `handle`. Lifetime is not managed here — `FileBacking`'s `Drop` is teardown.
pub trait FileOps: Send + Sync {
    fn kind(&self) -> FileKind;

    /// Distinguishes an `AF_UNIX` socket from an `AF_INET` one; both report
    /// [`FileKind::Socket`]. Comparing the ops singletons by pointer instead is
    /// unsound: distinct ZST statics may share an address.
    fn is_unix_socket(&self) -> bool {
        false
    }

    /// Returns bytes read on success, or a negative errno.
    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, offset: u64, flags: u32) -> isize;

    /// Returns bytes written on success, or a negative errno.
    fn write(&self, handle: usize, buf: &dyn IoBufRead, offset: u64, flags: u32) -> isize;

    /// Implementations MUST register the current task on the wait queue BEFORE
    /// checking readiness, so a wakeup arriving after registration finds it.
    fn poll_fused(&self, handle: usize, events: u16) -> FusedPollResult {
        // Register unconditionally: an `events == 0` caller still needs POLLHUP
        // and POLLERR wakeups, and the mask only filters on the wake side.
        let registered = self.poll_wait(handle);
        let revents = self.poll_events(handle, events);
        FusedPollResult {
            revents,
            registered,
            open_file_token: 0,
        }
    }

    /// **Legacy** — prefer `poll_fused` for new code.
    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let _ = (handle, events);
        0
    }

    /// **Legacy** — prefer `poll_fused` for new code.
    fn poll_wait(&self, handle: usize) -> bool {
        let _ = handle;
        false
    }

    /// **Legacy** — prefer `poll_fused` for new code.
    fn poll_unwait(&self, handle: usize) {
        let _ = handle;
    }

    fn stat(&self, handle: usize, out: &mut UserFsStat) -> i32 {
        let _ = (handle, out);
        -1
    }

    /// Notify subsystem that status flags (`O_NONBLOCK` etc.) changed.
    fn set_status_flags(&self, handle: usize, flags: u32) -> i32 {
        let _ = (handle, flags);
        0
    }

    fn ioctl(&self, handle: usize, cmd: u64, arg: u64) -> isize {
        let _ = (handle, cmd, arg);
        -1
    }

    fn seekable(&self) -> bool {
        false
    }

    fn size(&self, handle: usize) -> Option<u64> {
        let _ = handle;
        None
    }
}
