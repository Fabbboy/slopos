use core::ffi::c_int;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use slopos_ostd::lock_class;

use slopos_abi::KernelErrno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::{
    FS_TYPE_CHARDEV, FS_TYPE_FILE, O_ACCMODE, O_APPEND, O_CREAT, O_RDONLY, O_RDWR, O_WRONLY,
    UserFsStat,
};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{O_NOCTTY, O_NONBLOCK, POLLIN, POLLNVAL, POLLOUT, TtyIndex};
use slopos_ostd::KArc;
use slopos_ostd::KVec;
use slopos_ostd::process::quota::FileBacking;
use slopos_ostd::sync::{
    InitFlag, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, LockClassKey, SpinLock, SpinLockGuard,
};

use slopos_abi::quota::FdSlot;
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_kernel_services::driver_runtime::{
    current_task_controlling_tty, current_task_id, current_task_pgrp_handle, current_task_sid,
    set_current_task_controlling_tty,
};
use slopos_kernel_services::syscall_services::tty;
use slopos_mm::memory_layout_defs::MAX_PROCESSES;
use slopos_ostd::handle::Handle;
use slopos_ostd::process::quota::{Charge, Reservation, try_charge};
use slopos_ostd::process::{AccountId, Process, ProcessId, root_account};

/// Descriptors a process may hold at once — this kernel's `RLIMIT_NOFILE`, and
/// the length of every per-process descriptor table.
pub(super) const FILEIO_MAX_OPEN_FILES: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct OpenMode(u32);

impl OpenMode {
    pub const EMPTY: Self = Self(0);
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const CREAT: Self = Self(1 << 2);
    pub const APPEND: Self = Self(1 << 3);

    pub const fn contains(self, flag: Self) -> bool {
        (self.0 & flag.0) == flag.0 && flag.0 != 0
    }

    pub const fn intersects(self, flag: Self) -> bool {
        (self.0 & flag.0) != 0
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn from_bits(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn with_raw(self, raw: u32) -> Self {
        Self(self.0 | raw)
    }
}

impl Default for OpenMode {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl core::ops::BitOr for OpenMode {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for OpenMode {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl core::ops::BitAnd for OpenMode {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

pub(crate) fn posix_to_open_mode(posix: u32) -> OpenMode {
    let mut m = OpenMode::EMPTY;
    match posix & O_ACCMODE {
        O_RDONLY => m |= OpenMode::READ,
        O_WRONLY => m |= OpenMode::WRITE,
        O_RDWR => m |= OpenMode::READ | OpenMode::WRITE,
        _ => {}
    }
    if posix & O_CREAT != 0 {
        m |= OpenMode::CREAT;
    }
    if posix & O_APPEND != 0 {
        m |= OpenMode::APPEND;
    }
    m.with_raw(posix & !(O_ACCMODE | O_CREAT | O_APPEND))
}

pub(crate) fn openmode_to_posix_bits(mode: OpenMode) -> u32 {
    let mut posix = match (
        mode.contains(OpenMode::READ),
        mode.contains(OpenMode::WRITE),
    ) {
        (true, true) => O_RDWR,
        (false, true) => O_WRONLY,
        _ => O_RDONLY,
    };
    if mode.contains(OpenMode::APPEND) {
        posix |= O_APPEND;
    }
    posix |= mode.bits() & (O_NONBLOCK as u32 | O_NOCTTY as u32);
    posix
}

#[derive(Clone)]
pub struct PollRegInfo {
    /// Weak by design: a registration must never keep the file alive, and a
    /// failed upgrade drops a stale wakeup rather than reaching a reused slot.
    pub(super) open_file: slopos_ostd::KWeak<OpenFile>,
    pub registered: bool,
}

impl PollRegInfo {
    /// The empty weak never upgrades, so unregister is a no-op.
    pub fn none() -> Self {
        Self {
            open_file: slopos_ostd::KWeak::new(),
            registered: false,
        }
    }

    /// True once the open file has closed: the weak no longer upgrades, so the
    /// registration can never reach whatever reuses its fd number.
    pub fn is_stale(&self) -> bool {
        self.open_file.upgrade().is_none()
    }
}

/// POSIX's open file description. The [`KArc`]'s strong count is the dup/fork
/// alias count, and its `Drop` is the close: dropping the owned `backing` is
/// the teardown. Position and status flags live in atomics so aliases share
/// them per POSIX without taking a second lock.
pub(super) struct OpenFile {
    pub(super) ops: &'static dyn FileOps,
    pub(super) handle: usize,
    pub(super) position: AtomicU64,
    /// Held across the offset read, the I/O and the offset advance, so two
    /// writers sharing this description cannot resolve the same offset.
    pub(super) position_lock: SpinLock<()>,
    pub(super) status_flags: AtomicU32,
    /// Owned lifetime token for the subsystem object behind `handle`.
    /// `None` only for backings with no teardown (e.g. pidfd).
    #[expect(dead_code, reason = "held for ownership; dropping it is the teardown")]
    pub(super) backing: Option<KArc<dyn FileBacking>>,
}

impl OpenFile {
    pub(super) fn position(&self) -> u64 {
        self.position.load(Ordering::Acquire)
    }

    pub(super) fn status_flags(&self) -> OpenMode {
        OpenMode(self.status_flags.load(Ordering::Acquire))
    }

    pub(super) fn set_status_flags(&self, flags: OpenMode) {
        self.status_flags.store(flags.bits(), Ordering::Release);
    }
}

/// A strong reference holding an open file description alive outside any fd
/// table (SCM_RIGHTS in-flight custody, ring in-flight ops). Dropping it closes
/// that alias; installing it into an fd table transfers the alias. The
/// description — offset, status flags, backing — is shared, per POSIX.
pub struct FileRef {
    pub(super) open_file: KArc<OpenFile>,
}

impl FileRef {
    pub fn kind(&self) -> FileKind {
        self.open_file.ops.kind()
    }

    /// Another strong alias of this description. `FileRef` is deliberately not
    /// `Clone`, so aliasing stays an explicit act at the call site.
    pub fn alias(&self) -> FileRef {
        FileRef {
            open_file: self.open_file.clone(),
        }
    }

    /// True iff both name the same description — identity, not contents.
    pub fn ptr_eq(&self, other: &FileRef) -> bool {
        KArc::ptr_eq(&self.open_file, &other.open_file)
    }

    /// Live aliases plus fd-table entries. For lifetime assertions in tests.
    pub fn description_strong_count(&self) -> usize {
        KArc::strong_count(&self.open_file)
    }
}

/// Per-descriptor rights, stamped at creation and immutable thereafter.
///
/// Rights live in the table **entry**, never in the token. `Handle::from_parts`
/// is `pub const` and `Handle<T>` is unconditionally `Copy`, so a token is
/// forgeable and freely duplicable — it cannot carry authority. The entry is
/// what a lookup validates, which is this tree's equivalent of the
/// descriptor-lookup choke point Capsicum found it needed.
///
/// Stamped from the creating context and travelling **with** the entry, never
/// re-derived from whoever holds it now: a descriptor passed to a less
/// privileged process must not regain rights by being looked up there, and the
/// memfd clipboard's fd handoff already exercises that hazard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FdRights {
    /// May be aliased into another fd number (`dup`, `fork`, `CloneFd`).
    pub duplicate: bool,
    /// May be moved to another process (`SCM_RIGHTS`, `TransferFd`).
    pub transfer: bool,
}

impl FdRights {
    /// An ordinary descriptor: freely duplicated and passed.
    pub const ALL: Self = Self {
        duplicate: true,
        transfer: true,
    };

    /// A single-holder resource. Neither duplicable nor transferable, so the
    /// arbiter that revokes on holder death always names the one true holder.
    pub const LINEAR: Self = Self {
        duplicate: false,
        transfer: false,
    };

    /// The rights a descriptor of `kind` is created with.
    ///
    /// Total over `FileKind`, so a new kind must state its answer rather than
    /// inheriting a permissive default. This subsumes the standalone
    /// transferability predicate: `file_kind_transferable` answered one
    /// question, these answer both, and duplication is the one that was
    /// reachable through `dup`.
    pub const fn for_kind(kind: slopos_abi::file_ops::FileKind) -> Self {
        if slopos_abi::file_ops::file_kind_transferable(kind) {
            Self::ALL
        } else {
            Self::LINEAR
        }
    }
}

/// Per-descriptor inheritance policy, chosen by whoever installs the fd. Both
/// flags live on the fd number rather than the shared description, so two
/// aliases of one description can differ.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FdFlags {
    pub cloexec: bool,
    pub close_on_fork: bool,
}

impl FdFlags {
    /// Inherited by both `fork` and `exec` — the POSIX default for a
    /// descriptor opened without `O_CLOEXEC`.
    pub const NONE: Self = Self {
        cloexec: false,
        close_on_fork: false,
    };

    /// Neither `fork` nor `exec` carries it forward — for a descriptor whose
    /// kernel state is only meaningful to its creator.
    pub const PROCESS_PRIVATE: Self = Self {
        cloexec: true,
        close_on_fork: true,
    };
}

/// One file-descriptor-number → open-file mapping. Dropping one closes that
/// alias and refunds the slot.
///
/// Deliberately **not** `Clone`: a fork alias occupies a number in the
/// *child's* table and must be charged to the child's account, whereas a
/// derived `Clone` would copy the parent's token and refund the parent twice.
/// The alias paths call [`try_alias`](Self::try_alias) instead.
pub(super) struct FdEntry {
    pub(super) open_file: KArc<OpenFile>,
    pub(super) cloexec: bool,
    pub(super) close_on_fork: bool,
    /// Stamped at creation from the description's kind, immutable, and carried
    /// by every alias. Never re-derived at lookup time.
    pub(super) rights: FdRights,
    /// Refunded by this struct's own `Drop`, with the amount it holds rather
    /// than one recomputed at the refund site.
    slot_charge: Charge<FdSlot>,
}

impl FdEntry {
    pub(super) fn new(
        open_file: KArc<OpenFile>,
        flags: FdFlags,
        reservation: Reservation<FdSlot>,
    ) -> Self {
        let rights = FdRights::for_kind(open_file.ops.kind());
        Self {
            open_file,
            cloexec: flags.cloexec,
            close_on_fork: flags.close_on_fork,
            rights,
            slot_charge: Charge::commit(reservation),
        }
    }

    /// Another descriptor number naming the same open file, charged to
    /// `account`. `None` when that account has no room, or when this
    /// descriptor may not be duplicated.
    ///
    /// The rights check is here rather than at each caller because this is the
    /// one function every aliasing path funnels through — `dup`, `fork`, and
    /// the spawn `CloneFd` arm all reach it. A check at the callers is a check
    /// that can be forgotten by the next one added.
    pub(super) fn try_alias(&self, account: AccountId) -> Option<Self> {
        if !self.rights.duplicate {
            return None;
        }
        let reservation = try_charge::<FdSlot>(account, 1).ok()?;
        Some(Self::new(
            self.open_file.clone(),
            FdFlags {
                cloexec: self.cloexec,
                close_on_fork: self.close_on_fork,
            },
            reservation,
        ))
    }

    /// Point this descriptor number at a different open file, keeping the
    /// charge it already holds and handing back the description it displaced
    /// so the caller can drop it off-lock. The number stays occupied
    /// throughout, so exactly one charge exists for it at every instant.
    pub(super) fn replacing(
        self,
        open_file: KArc<OpenFile>,
        close_on_fork: bool,
    ) -> (Self, KArc<OpenFile>) {
        let displaced = self.open_file;
        (
            Self {
                open_file: open_file.clone(),
                cloexec: self.cloexec,
                close_on_fork,
                // Re-derived from the NEW description: the number is being
                // repointed, so the rights are the incoming file's, not the
                // displaced one's.
                rights: FdRights::for_kind(open_file.ops.kind()),
                slot_charge: self.slot_charge,
            },
            displaced,
        )
    }
}

// A slot's index *is* the owning process's registry slot — the slot space the
// process registry and `slopos_mm`'s address-space table share — so a table is
// found by indexing rather than scanning. `generation` is what makes that
// sound: ids recycle, and `process_id` is only a display value and the
// occupancy sentinel for the lock-free peek.
//
// Lock order: per-process `slot.inner` (REGISTRY) first, `OPEN_FILES_STATE`
// (RESOURCE) second. A holder of two different per-process locks must snapshot
// one and release before taking the other (fork, in fdtable.rs).

pub(super) struct FileTableSlot {
    pub(super) process_id: AtomicU32,
    /// Written before `process_id` is published and cleared after it, so a
    /// reader that sees an occupied slot sees the matching generation.
    pub(super) generation: AtomicU64,
    pub(super) inner: SpinLock<FileTableSlotInner>,
}

pub(super) struct FileTableSlotInner {
    pub(super) in_use: bool,
    /// Empty until the slot is claimed. Heap-backed rather than inline so the
    /// `PROCESS_TABLES` spine does not scale descriptor capacity by
    /// `MAX_PROCESSES`, and so the array is built and freed off the slot lock.
    pub(super) descriptors: KVec<Option<FdEntry>>,
}

/// A zero-filled descriptor array, built before any slot lock is taken. Every
/// caller is a process-creation path that already fails creation when it cannot
/// get a slot, so there is nowhere this has to succeed.
pub(super) fn new_descriptor_table() -> Option<KVec<Option<FdEntry>>> {
    let mut table = KVec::with_capacity(FILEIO_MAX_OPEN_FILES).ok()?;
    for _ in 0..FILEIO_MAX_OPEN_FILES {
        table.push(None).ok()?;
    }
    Some(table)
}

impl FileTableSlot {
    /// The lock class comes from the caller: minted here, the kernel table and
    /// the process tables would share one class and an inversion between them
    /// could not be seen. The process tables do share a class — one
    /// declaration, many like instances.
    pub(super) const fn new(in_use: bool, class: &'static LockClassKey) -> Self {
        Self {
            process_id: AtomicU32::new(INVALID_PROCESS_ID),
            generation: AtomicU64::new(0),
            inner: SpinLock::new(
                FileTableSlotInner {
                    in_use,
                    descriptors: KVec::new(),
                },
                class,
            ),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct ExternalOpsState {
    pub(super) tty_ops: Option<&'static dyn FileOps>,
    pub(super) socket_ops: Option<&'static dyn FileOps>,
}

impl ExternalOpsState {
    const fn new() -> Self {
        Self {
            tty_ops: None,
            socket_ops: None,
        }
    }
}

/// Shared fileio registry state: the external `FileOps` singletons, set once at
/// subsystem init, plus the init latch. Liveness lives in each
/// [`KArc<OpenFile>`]'s strong count, not in a table here.
pub(super) struct OpenFilesState {
    pub(super) initialized: bool,
    pub(super) external_ops: ExternalOpsState,
}

impl OpenFilesState {
    const fn uninitialized() -> Self {
        Self {
            initialized: false,
            external_ops: ExternalOpsState::new(),
        }
    }
}

pub(super) static KERNEL_TABLE: FileTableSlot =
    FileTableSlot::new(true, lock_class!("KERNEL_FILE_TABLE", LOCK_LEVEL_REGISTRY));
pub(super) static PROCESS_TABLES: [FileTableSlot; MAX_PROCESSES] = [const {
    FileTableSlot::new(
        false,
        lock_class!("PROCESS_FILE_TABLE", LOCK_LEVEL_REGISTRY),
    )
}; MAX_PROCESSES];
/// Ranked above the filesystem locks it is held across, and `LO_DUPOK`
/// because distinct open descriptions are distinct instances of one class:
/// a caller only ever holds one at a time.
pub(super) const OPEN_FILE_POSITION_CLASS: &LockClassKey = lock_class!(
    "OPEN_FILE_POSITION",
    LOCK_LEVEL_REGISTRY,
    slopos_ostd::sync::lock_tracking::LO_DUPOK
);

pub(super) static OPEN_FILES_STATE: SpinLock<OpenFilesState> = SpinLock::new(
    OpenFilesState::uninitialized(),
    lock_class!("OPEN_FILES_STATE", LOCK_LEVEL_RESOURCE),
);
pub(super) static FILEIO_INIT: InitFlag = InitFlag::new();

/// The descriptor table `process` owns — one index, no scan. A slot rebound
/// since the handle was minted answers `None`, not the new occupant's table.
pub(super) fn slot_for_process(process: Handle<Process>) -> Option<&'static FileTableSlot> {
    let slot = PROCESS_TABLES.get(process.slot() as usize)?;
    if slot.process_id.load(Ordering::Acquire) == INVALID_PROCESS_ID
        || slot.generation.load(Ordering::Acquire) != process.generation()
    {
        return None;
    }
    Some(slot)
}

/// Which descriptor table an operation acts on.
///
/// The kernel's table is a variant a caller has to name, so no failed pid
/// lookup, zeroed field or forgotten argument reaches it by omission. The
/// process case carries a [`ProcessId`], so the lookup is a generation-checked
/// index rather than a scan for a matching number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdTable {
    /// The kernel's own descriptors, shared by every kernel task.
    Kernel,
    Process(ProcessId),
}

impl FdTable {
    /// The table a live process owns.
    #[inline]
    pub fn of(process: &Process) -> Option<Self> {
        ProcessId::of(process).map(Self::Process)
    }

    /// Resolve a numeric id to the table its process owns. `None` for an id
    /// naming no live process — never [`FdTable::Kernel`], which would hand a
    /// user process the kernel's descriptors.
    #[inline]
    pub fn resolve(id: u32) -> Option<Self> {
        ProcessId::resolve(id).map(Self::Process)
    }

    /// The owning process, or `None` for the kernel table.
    #[inline]
    pub fn process(self) -> Option<ProcessId> {
        match self {
            Self::Kernel => None,
            Self::Process(process) => Some(process),
        }
    }

    /// The owning process's handle, or `None` for the kernel table.
    #[inline]
    pub fn handle(self) -> Option<Handle<Process>> {
        match self {
            Self::Kernel => None,
            Self::Process(process) => Some(process.handle()),
        }
    }

    /// The numeric id, for logs and the syscall ABI. `INVALID_PROCESS_ID` for
    /// the kernel table, which owns no process id.
    #[inline]
    pub fn id(self) -> u32 {
        match self {
            Self::Kernel => INVALID_PROCESS_ID,
            Self::Process(process) => process.id(),
        }
    }

    /// The account a descriptor number in this table is charged to.
    ///
    /// The kernel's own table names the root account **explicitly**, never as a
    /// lookup-failure fallback: a reaped process answers with its own now-dark
    /// account, whose charges are vacuous, rather than billing the root.
    #[inline]
    pub fn account(self) -> AccountId {
        match self {
            Self::Kernel => root_account(),
            Self::Process(process) => process
                .get()
                .map_or(AccountId::NONE, |process| process.account()),
        }
    }
}

/// The slot backing `table` — one index for a process, one static for the
/// kernel, no scan either way.
pub(super) fn slot_for_table(table: FdTable) -> Option<&'static FileTableSlot> {
    match table {
        FdTable::Kernel => Some(&KERNEL_TABLE),
        FdTable::Process(process) => slot_for_process(process.handle()),
    }
}

/// Lazy-initialise the open-files registry and run `f` with it locked.
pub(super) fn with_open_files<R>(f: impl FnOnce(&mut OpenFilesState) -> R) -> R {
    let mut guard = OPEN_FILES_STATE.lock();
    if !guard.initialized {
        FILEIO_INIT.init_once();
        guard.initialized = true;
    }
    f(&mut *guard)
}

/// Lock `table`'s slot and run `f` with mutable access to its descriptors.
/// `None` if the table does not exist or was claimed but is not yet `in_use`.
pub(super) fn with_table_slot<R>(
    table: FdTable,
    f: impl FnOnce(&mut FileTableSlotInner) -> R,
) -> Option<R> {
    let slot = slot_for_table(table)?;
    let mut guard = slot.inner.lock();
    if !guard.in_use {
        return None;
    }
    Some(f(&mut *guard))
}

/// Acquire the slot lock and return the guard, or `None` when the table does
/// not exist. A lookup, never an allocation: a table is minted only by an
/// explicit create/clone, which fails process creation when no slot is free.
pub(super) fn lock_table_slot(
    table: FdTable,
) -> Option<SpinLockGuard<'static, FileTableSlotInner>> {
    let slot = slot_for_table(table)?;
    let guard = slot.inner.lock();
    if !guard.in_use {
        return None;
    }
    Some(guard)
}

/// A strong [`KArc<OpenFile>`] captured under the per-process slot lock, so I/O
/// proceeds off-lock and a concurrent `close` dropping the fd-table alias
/// cannot tear the file down mid-operation. Position and status flags are read
/// live from its atomics — the intended shared-offset POSIX behaviour.
pub(super) struct FdSnapshot {
    pub(super) open_file: KArc<OpenFile>,
    /// Copied from the entry at lookup, so every caller that snapshots an fd
    /// also gets what may be done with it. `snapshot_fd` and `get_fd_entry`
    /// are the two lookup choke points, which is where the plan puts the test.
    pub(super) rights: FdRights,
}

impl FdSnapshot {
    pub(super) fn ops(&self) -> &'static dyn FileOps {
        self.open_file.ops
    }

    pub(super) fn handle(&self) -> usize {
        self.open_file.handle
    }

    pub(super) fn position(&self) -> u64 {
        self.open_file.position()
    }

    pub(super) fn status_flags(&self) -> OpenMode {
        self.open_file.status_flags()
    }
}

pub(super) fn snapshot_fd(inner: &FileTableSlotInner, fd: c_int) -> Option<FdSnapshot> {
    let entry = get_fd_entry(inner, fd)?;
    Some(FdSnapshot {
        open_file: entry.open_file.clone(),
        rights: entry.rights,
    })
}

pub(super) fn get_fd_entry(inner: &FileTableSlotInner, fd: c_int) -> Option<&FdEntry> {
    if fd < 0 || fd as usize >= inner.descriptors.len() {
        return None;
    }
    inner.descriptors[fd as usize].as_ref()
}

pub(super) fn get_fd_entry_mut(inner: &mut FileTableSlotInner, fd: c_int) -> Option<&mut FdEntry> {
    if fd < 0 || fd as usize >= inner.descriptors.len() {
        return None;
    }
    inner.descriptors[fd as usize].as_mut()
}

pub(super) fn find_free_slot(inner: &FileTableSlotInner) -> Option<usize> {
    find_free_slot_from(inner, 0)
}

pub(super) fn find_free_slot_from(inner: &FileTableSlotInner, min_fd: usize) -> Option<usize> {
    for idx in min_fd..inner.descriptors.len() {
        if inner.descriptors[idx].is_none() {
            return Some(idx);
        }
    }
    None
}

/// Build a `KArc<OpenFile>` owning `backing`. On allocation failure `backing`
/// has already been consumed and dropped — its teardown has run.
pub(super) fn new_open_file(
    ops: &'static dyn FileOps,
    handle: usize,
    status_flags: OpenMode,
    position: u64,
    backing: Option<KArc<dyn FileBacking>>,
) -> Option<KArc<OpenFile>> {
    KArc::try_new(OpenFile {
        ops,
        handle,
        position: AtomicU64::new(position),
        position_lock: SpinLock::new((), OPEN_FILE_POSITION_CLASS),
        status_flags: AtomicU32::new(status_flags.bits()),
        backing,
    })
    .ok()
}

pub(super) fn parse_pts_path(path: &[u8]) -> Option<TtyIndex> {
    let rest = path.strip_prefix(b"/dev/pts/")?;
    if rest.is_empty() {
        return None;
    }

    let mut value = 0u16;
    for &byte in rest {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as u16)?;
    }

    let idx = value as u8;
    if value > u8::MAX as u16 || idx < 2 {
        return None;
    }

    Some(TtyIndex(idx))
}

pub(super) fn maybe_acquire_controlling_tty_on_open(tty_idx: TtyIndex, flags: u32) {
    if (flags & O_NOCTTY as u32) != 0 {
        return;
    }

    let task_id = current_task_id();
    let sid = current_task_sid();
    if task_id == 0 || sid == 0 || sid != task_id {
        return;
    }
    if current_task_controlling_tty().is_some() {
        return;
    }

    let fg = current_task_pgrp_handle().unwrap_or_else(slopos_ostd::KWeak::new);
    if tty::acquire_controlling_terminal(tty_idx, fg).is_ok() {
        let _ = set_current_task_controlling_tty(Some(tty_idx));
    }
}

pub fn fileio_register_tty_ops(ops: &'static dyn FileOps) {
    with_open_files(|state| state.external_ops.tty_ops = Some(ops));
}

pub fn fileio_register_socket_ops(ops: &'static dyn FileOps) {
    with_open_files(|state| state.external_ops.socket_ops = Some(ops));
}

struct LocalTtyOps;

static LOCAL_TTY_OPS: LocalTtyOps = LocalTtyOps;

fn local_tty_index(handle: usize) -> Result<TtyIndex, slopos_abi::Errno> {
    if handle > u8::MAX as usize {
        Err(slopos_abi::Errno::EBADF)
    } else {
        Ok(TtyIndex(handle as u8))
    }
}

impl FileOps for LocalTtyOps {
    fn kind(&self) -> FileKind {
        FileKind::Tty
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, flags: u32) -> isize {
        let tty_idx = match local_tty_index(handle) {
            Ok(idx) => idx,
            Err(e) => return e.as_isize(),
        };
        let nonblock = (flags & O_NONBLOCK as u32) != 0;
        let buf_len = buf.len();
        let mut tmp = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return slopos_abi::Errno::ENOMEM.as_isize(),
        };
        let read_len = buf_len.min(tmp.len());
        match tty::read_cooked(tty_idx, tmp.as_mut_ptr(), read_len, nonblock) {
            Ok(n) => match buf.copy_in(0, &tmp[..n]) {
                Ok(written) => written as isize,
                Err(e) => e.as_isize(),
            },
            Err(e) => e.to_errno() as isize,
        }
    }

    fn write(&self, handle: usize, buf: &dyn IoBufRead, _offset: u64, flags: u32) -> isize {
        let tty_idx = match local_tty_index(handle) {
            Ok(idx) => idx,
            Err(e) => return e.as_isize(),
        };
        let nonblock = (flags & O_NONBLOCK as u32) != 0;
        let mut staging = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return slopos_abi::Errno::ENOMEM.as_isize(),
        };
        let buf_len = buf.len();
        let mut total = 0usize;

        while total < buf_len {
            let chunk = (buf_len - total).min(staging.len());
            let n = match buf.copy_out(total, &mut staging[..chunk]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        e.as_isize()
                    };
                }
            };
            match tty::write_bytes(tty_idx, staging.as_ptr(), n, nonblock) {
                Ok(written) => {
                    total += written;
                    if written < n {
                        break;
                    }
                }
                Err(e) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        e.to_errno() as isize
                    };
                }
            }
        }
        total as isize
    }

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        let tty_idx = match local_tty_index(handle) {
            Ok(idx) => idx,
            Err(_) => {
                return slopos_abi::file_ops::FusedPollResult {
                    revents: 0,
                    registered: false,
                    open_file_token: 0,
                };
            }
        };
        let registered = tty::poll_enqueue(tty_idx);
        let revents = tty::poll_events(tty_idx, events);
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_token: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        match local_tty_index(handle) {
            Ok(idx) => tty::poll_events(idx, events),
            Err(_) => 0,
        }
    }

    fn poll_wait(&self, handle: usize) -> bool {
        match local_tty_index(handle) {
            Ok(idx) => tty::poll_enqueue(idx),
            Err(_) => false,
        }
    }

    fn poll_unwait(&self, handle: usize) {
        if let Ok(idx) = local_tty_index(handle) {
            tty::poll_dequeue(idx);
        }
    }

    fn stat(&self, _handle: usize, out: &mut UserFsStat) -> i32 {
        out.type_ = FS_TYPE_CHARDEV;
        out.size = 0;
        0
    }
}

pub(super) fn external_tty_ops(external_ops: &ExternalOpsState) -> Option<&'static dyn FileOps> {
    external_ops.tty_ops
}

pub(super) fn effective_tty_ops(external_ops: &ExternalOpsState) -> &'static dyn FileOps {
    external_tty_ops(external_ops).unwrap_or(&LOCAL_TTY_OPS)
}

pub(super) fn external_socket_ops(external_ops: &ExternalOpsState) -> Option<&'static dyn FileOps> {
    external_ops.socket_ops
}

pub(super) fn kind_is_tty(kind: FileKind) -> bool {
    kind == FileKind::Tty
}

mod fdops;
mod fdtable;
mod poll;

pub use fdops::*;
pub use fdtable::*;
pub use poll::*;
