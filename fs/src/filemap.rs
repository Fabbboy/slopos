//! Per-inode page sets for file-backed `mmap` (G14).
//!
//! **A per-inode page set is the authority for the pages it holds while a
//! shared mapping is live.** Nothing here maps an ext2 `BlockCache` frame, so
//! the block cache stays the single device-facing cache, at the cost of one
//! 4 KiB copy per mapped page. `read(2)` and `write(2)` are routed through the
//! set for the ranges it covers ([`read_through`], [`write_through`]), and
//! writeback goes out through [`FileSystem::write`], inheriting the
//! filesystem's own ordering.
//!
//! Pages are populated at `mmap(2)` time, in syscall context. The page-fault
//! handler runs on IST4 with interrupts off and cannot take `CACHED_EXT2` or
//! park on a virtio completion, so nothing can be faulted in later — which is
//! also why a mapping reaching past EOF is refused rather than deferring a
//! `SIGBUS` this kernel has no path for.
//!
//! A writeback writes back *every* page of a set a shared mapping reached: a
//! user store sets the CPU's PTE dirty bit and nothing in this kernel harvests
//! it. A set that only ever served a `MAP_PRIVATE` copy holds exactly what the
//! filesystem holds, so its writeback is skipped.
//!
//! When an inode's name is removed the VFS flushes its page set and then
//! unkeys it ([`detach_inode`]), *before* the filesystem performs the removal.
//! ext2 inode numbers carry no generation, so unkeying at the one moment the
//! number can be reused is what stops a reallocated inode from resolving to
//! the previous file's pages. A live mapping keeps reading the frames; its
//! stores are no longer written back, because the blocks they would reach may
//! already belong to something else.
//!
//! Lock order: [`FILEMAP_IO`] (sleeping, held across the filesystem calls) →
//! [`FILEMAP`] (spinning; every operation under it is a bounded scan or a page
//! copy) → `CACHED_EXT2`. [`release`] is reached from process teardown under a
//! preempt guard, so it may not block, allocate or reach the filesystem: it
//! queues, and [`drain_pending`] — called at the head of [`acquire`], by
//! `munmap` and `exec` once the address-space lock is dropped, and by the ext2
//! flusher — completes the work.

use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_abi::Errno;
use slopos_abi::addr::PhysAddr;
use slopos_mm::filemap_hook::FileMapOps;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::page_alloc::alloc_kernel_page;
use slopos_mm::vma_region::FileMapRef;
use slopos_ostd::mm::frame::{claim_owned_anon_page, release_owned_anon_page};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, Mutex, SpinLock};
use slopos_ostd::{KVec, klog_info, lock_class};

use crate::vfs::traits::same_filesystem;
use crate::vfs::{FileSystem, InodeId};

const PAGE_SIZE: u64 = 4096;
const PAGE_SIZE_USIZE: usize = 4096;

/// Inodes that may hold a page set at once.
const MAX_MAPPED_INODES: usize = 16;

/// Pages every set may hold between them — 4 MiB. A page under a live user PTE
/// is unreclaimable by construction, so this ceiling is the only bound on them.
const MAX_MAPPED_PAGES: u32 = 1024;

/// Why a page set could not be handed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMapError {
    /// Every page-set slot is in use.
    TooManyInodes,
    /// The registry's page ceiling.
    TooManyPages,
    NoMemory,
    /// The request covers no page.
    EmptyRange,
    /// The mapping would reach past the end of the file.
    PastEof,
    /// The handle names a slot that has been recycled.
    Stale,
    /// The inode refuses to be written: a writable page set on it would
    /// publish bytes the filesystem will never accept.
    WriteRefused,
    Io,
    Interrupted,
}

impl FileMapError {
    pub fn to_errno(self) -> Errno {
        match self {
            Self::TooManyInodes | Self::TooManyPages | Self::NoMemory => Errno::ENOMEM,
            Self::EmptyRange | Self::PastEof | Self::Stale => Errno::EINVAL,
            Self::WriteRefused => Errno::EACCES,
            Self::Io => Errno::EIO,
            Self::Interrupted => Errno::EINTR,
        }
    }
}

/// One inode's pages. A free slot has `fs == None`.
struct PageSet {
    fs: Option<&'static dyn FileSystem>,
    inode: InodeId,
    /// File-relative index of `pages[0]`.
    first_page: u64,
    pages: KVec<PhysAddr>,
    /// Mapping references, counted in pages, plus the one unit an in-flight
    /// [`acquire`] holds until its caller has mapped or given up.
    refs: u32,
    generation: u32,
    /// Cleared while the pages are being populated: until then the filesystem
    /// is still the authority, and `read`/`write` fall through to it.
    ready: bool,
    /// A shared mapping or a `write(2)` reached this set, so its pages may
    /// differ from the filesystem's.
    dirtyable: bool,
    /// The last reference went; writeback and the frame frees are owed.
    pending_release: bool,
    /// `msync(MS_ASYNC)`: writeback is owed, the set stays.
    pending_flush: bool,
    /// The inode's last name is going away: the set stops being findable by
    /// `(filesystem, inode)` and stops being written back, while every live
    /// mapping keeps reading it.
    forgotten: bool,
}

impl PageSet {
    const EMPTY: Self = Self {
        fs: None,
        inode: 0,
        first_page: 0,
        pages: KVec::new(),
        refs: 0,
        generation: 0,
        ready: false,
        dirtyable: false,
        pending_release: false,
        pending_flush: false,
        forgotten: false,
    };

    /// Deliberately `false` once forgotten: the inode number may already have
    /// been reallocated to a different file.
    fn holds(&self, fs: &'static dyn FileSystem, inode: InodeId) -> bool {
        match self.fs {
            Some(mine) => !self.forgotten && self.inode == inode && same_filesystem(mine, fs),
            None => false,
        }
    }

    fn covers(&self, first_page: u64, page_count: u32) -> bool {
        first_page >= self.first_page
            && first_page + page_count as u64 <= self.first_page + self.pages.len() as u64
    }
}

/// Sleeping, because populate and writeback reach the filesystem; holding it
/// is what makes "populate, then publish" indivisible.
static FILEMAP_IO: Mutex<()> = Mutex::new((), lock_class!("FILEMAP_IO", LOCK_LEVEL_RESOURCE));

/// Spinning, and must stay so: [`release`] runs from a `Drop` the task-exit
/// path reaches under a preempt guard. No filesystem call is made under it.
static FILEMAP: SpinLock<[PageSet; MAX_MAPPED_INODES]> = SpinLock::new(
    [const { PageSet::EMPTY }; MAX_MAPPED_INODES],
    lock_class!("FILEMAP", LOCK_LEVEL_RESOURCE),
);

/// Sets owing writeback, so a flusher's wait predicate takes no lock.
static PENDING: AtomicUsize = AtomicUsize::new(0);

fn io_lock() -> Result<slopos_ostd::sync::MutexGuard<'static, ()>, FileMapError> {
    FILEMAP_IO.lock().map_err(|_| FileMapError::Interrupted)
}

fn ref_for(slot: usize, generation: u32) -> FileMapRef {
    FileMapRef {
        slot: slot as u16,
        generation,
    }
}

fn resolve(sets: &mut [PageSet], map: FileMapRef) -> Option<&mut PageSet> {
    let entry = sets.get_mut(map.slot as usize)?;
    if entry.fs.is_none() || entry.generation != map.generation {
        return None;
    }
    Some(entry)
}

/// What [`acquire`] must do after it drops the bookkeeping lock.
struct AcquirePlan {
    slot: usize,
    generation: u32,
    /// The page range the set must end up holding, when it does not already.
    grow: Option<(u64, u32)>,
    /// Paddrs the set already holds, in `grow.0`-relative order, for the
    /// pages the union keeps.
    kept_first: u64,
    kept: KVec<PhysAddr>,
    /// A slot claimed by this call; a failure must give it back.
    fresh: bool,
}

/// Claim a page set for `[first_page, first_page + page_count)` of `inode`,
/// populating whatever it does not already hold, and answer the physical
/// addresses of exactly that range.
///
/// A writable set on a sealed inode is refused here as well as by `open(2)`:
/// the set is what `read(2)` is routed through, so bytes stored into it are
/// published to every reader of the file.
///
/// The returned [`FileMapRef`] carries one reference unit, which the caller
/// releases once it has mapped the pages or given up, leaving the mapping's
/// own per-page references behind.
pub fn acquire(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    first_page: u64,
    page_count: u32,
    writable: bool,
) -> Result<(FileMapRef, KVec<u64>), FileMapError> {
    if page_count == 0 {
        return Err(FileMapError::EmptyRange);
    }
    if page_count > MAX_MAPPED_PAGES {
        return Err(FileMapError::TooManyPages);
    }
    // A set queued by a process that exited has nobody else to complete it on
    // a boot that runs no ext2 flusher, and the budget it holds would refuse
    // every later mapping.
    drain_pending();
    if writable && fs.stat(inode).map_err(|_| FileMapError::Io)?.sealed {
        return Err(FileMapError::WriteRefused);
    }
    let _io = io_lock()?;

    let plan = plan_acquire(fs, inode, first_page, page_count)?;
    if let Some((union_first, union_count)) = plan.grow {
        match populate(fs, inode, &plan, union_first, union_count) {
            Ok(pages) => install(&plan, union_first, pages),
            Err(e) => {
                abandon(&plan);
                return Err(e);
            }
        }
    }

    let paddrs = match snapshot_range(&plan, first_page, page_count) {
        Ok(paddrs) => paddrs,
        Err(e) => {
            // The reference `plan_acquire` took: without this the set stays
            // charged with no handle left to release it.
            release(ref_for(plan.slot, plan.generation), 1);
            return Err(e);
        }
    };
    Ok((ref_for(plan.slot, plan.generation), paddrs))
}

/// The bookkeeping half of [`acquire`]: reserve the slot and the page budget,
/// take the caller's reference, and say what is left to read.
#[inline(never)]
fn plan_acquire(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    first_page: u64,
    page_count: u32,
) -> Result<AcquirePlan, FileMapError> {
    let mut sets = FILEMAP.lock();

    let mut held = 0u32;
    let mut existing = None;
    let mut free = None;
    for (idx, entry) in sets.iter().enumerate() {
        held = held.saturating_add(entry.pages.len() as u32);
        if entry.holds(fs, inode) {
            existing = Some(idx);
        } else if entry.fs.is_none() && free.is_none() {
            free = Some(idx);
        }
    }

    let (slot, fresh) = match existing {
        Some(idx) => (idx, false),
        None => (free.ok_or(FileMapError::TooManyInodes)?, true),
    };

    let (union_first, union_count) = if fresh {
        (first_page, page_count)
    } else {
        let entry = &sets[slot];
        if entry.ready && entry.covers(first_page, page_count) {
            let generation = entry.generation;
            let grown = None;
            let kept = KVec::new();
            sets[slot].refs = sets[slot].refs.saturating_add(1);
            revive(&mut sets[slot]);
            return Ok(AcquirePlan {
                slot,
                generation,
                grow: grown,
                kept_first: 0,
                kept,
                fresh: false,
            });
        }
        let end = (entry.first_page + entry.pages.len() as u64).max(first_page + page_count as u64);
        let start = entry.first_page.min(first_page);
        let count = u32::try_from(end - start).map_err(|_| FileMapError::TooManyPages)?;
        (start, count)
    };

    let already = sets[slot].pages.len() as u32;
    if union_count > already && held.saturating_add(union_count - already) > MAX_MAPPED_PAGES {
        return Err(FileMapError::TooManyPages);
    }

    let kept_first = sets[slot].first_page;
    let mut kept: KVec<PhysAddr> = KVec::new();
    for pa in sets[slot].pages.iter() {
        kept.push(*pa).map_err(|_| FileMapError::NoMemory)?;
    }

    let entry = &mut sets[slot];
    if fresh {
        entry.fs = Some(fs);
        entry.inode = inode;
        entry.first_page = union_first;
        entry.generation = entry.generation.wrapping_add(1);
        entry.ready = false;
        entry.dirtyable = false;
    }
    entry.refs = entry.refs.saturating_add(1);
    revive(entry);

    Ok(AcquirePlan {
        slot,
        generation: entry.generation,
        grow: Some((union_first, union_count)),
        kept_first,
        kept,
        fresh,
    })
}

/// A set queued for writeback is revived rather than replaced: it still holds
/// the authoritative bytes for its pages.
fn revive(entry: &mut PageSet) {
    if entry.pending_release || entry.pending_flush {
        entry.pending_release = false;
        entry.pending_flush = false;
        PENDING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Allocate and fill the union's pages, reusing every frame the set already
/// holds. Reaches the filesystem, so it runs with only [`FILEMAP_IO`] held.
#[inline(never)]
fn populate(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    plan: &AcquirePlan,
    union_first: u64,
    union_count: u32,
) -> Result<KVec<PhysAddr>, FileMapError> {
    let size = fs.stat(inode).map_err(|_| FileMapError::Io)?.size;
    // A page wholly past EOF has nothing to hold, and this kernel has no
    // SIGBUS path to defer the question to.
    if union_first.saturating_add(union_count as u64 - 1) * PAGE_SIZE >= size {
        return Err(FileMapError::PastEof);
    }

    let mut staging = KVec::<u8>::zeroed(PAGE_SIZE_USIZE).map_err(|_| FileMapError::NoMemory)?;
    let mut pages: KVec<PhysAddr> = KVec::new();
    for i in 0..union_count {
        let page = union_first + i as u64;
        let reused = plan
            .kept
            .get(usize::try_from(page.wrapping_sub(plan.kept_first)).unwrap_or(usize::MAX))
            .copied();
        let result = match reused {
            Some(pa) => Ok(pa),
            None => claim_page().and_then(|pa| {
                match read_page_into(fs, inode, page, size, pa, staging.as_mut_slice()) {
                    Ok(()) => Ok(pa),
                    Err(e) => {
                        release_owned_anon_page(pa);
                        Err(e)
                    }
                }
            }),
        };
        match result {
            Ok(pa) => {
                if pages.push(pa).is_err() {
                    if reused.is_none() {
                        release_owned_anon_page(pa);
                    }
                    free_new_pages(plan, &pages);
                    return Err(FileMapError::NoMemory);
                }
            }
            Err(e) => {
                free_new_pages(plan, &pages);
                return Err(e);
            }
        }
    }
    Ok(pages)
}

/// Give back the frames a failed [`populate`] allocated, leaving the ones the
/// set already owned alone.
fn free_new_pages(plan: &AcquirePlan, pages: &KVec<PhysAddr>) {
    for pa in pages.iter() {
        if !plan.kept.iter().any(|k| *k == *pa) {
            release_owned_anon_page(*pa);
        }
    }
}

/// One owned frame, claimed the way a memfd claims its pages, so it outlives
/// every mapping of it and aliasing it into a user PTE is legal.
fn claim_page() -> Result<PhysAddr, FileMapError> {
    let pa = alloc_kernel_page();
    if pa.is_null() {
        return Err(FileMapError::NoMemory);
    }
    if !claim_owned_anon_page(pa) {
        klog_info!("filemap: allocator returned a non-UNUSED frame; leaking it");
        return Err(FileMapError::NoMemory);
    }
    Ok(pa)
}

/// Read one file page into `pa`, zero-filling past EOF.
#[inline(never)]
fn read_page_into(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    page: u64,
    size: u64,
    pa: PhysAddr,
    staging: &mut [u8],
) -> Result<(), FileMapError> {
    let offset = page * PAGE_SIZE;
    let want = usize::try_from((size - offset).min(PAGE_SIZE)).unwrap_or(PAGE_SIZE_USIZE);
    staging.fill(0);
    let mut done = 0usize;
    while done < want {
        match fs.read(inode, offset + done as u64, &mut staging[done..want]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(_) => return Err(FileMapError::Io),
        }
    }
    let virt = pa.try_to_virt().ok_or(FileMapError::Io)?;
    if !slopos_ostd::mm::hhdm_bytes::write_bytes(virt, 0, staging) {
        return Err(FileMapError::Io);
    }
    Ok(())
}

/// Publish a populated union.
fn install(plan: &AcquirePlan, union_first: u64, pages: KVec<PhysAddr>) {
    let mut sets = FILEMAP.lock();
    let entry = &mut sets[plan.slot];
    if entry.generation != plan.generation {
        drop(sets);
        free_new_pages(plan, &pages);
        return;
    }
    entry.first_page = union_first;
    entry.pages = pages;
    entry.ready = true;
}

/// Undo [`plan_acquire`] after a failed populate.
fn abandon(plan: &AcquirePlan) {
    let mut sets = FILEMAP.lock();
    let entry = &mut sets[plan.slot];
    if entry.generation != plan.generation {
        return;
    }
    entry.refs = entry.refs.saturating_sub(1);
    if entry.refs != 0 {
        return;
    }
    if plan.fresh {
        entry.fs = None;
        entry.pages = KVec::new();
        entry.ready = false;
        return;
    }
    // Reviving the set to take this reference dropped its release obligation;
    // handing the last reference back has to put it back, or nothing frees it.
    if !entry.pending_release {
        if entry.pending_flush {
            entry.pending_flush = false;
        } else {
            PENDING.fetch_add(1, Ordering::Relaxed);
        }
        entry.pending_release = true;
    }
}

fn snapshot_range(
    plan: &AcquirePlan,
    first_page: u64,
    page_count: u32,
) -> Result<KVec<u64>, FileMapError> {
    let sets = FILEMAP.lock();
    let entry = &sets[plan.slot];
    if entry.generation != plan.generation || !entry.covers(first_page, page_count) {
        return Err(FileMapError::Stale);
    }
    let base = usize::try_from(first_page - entry.first_page).unwrap_or(usize::MAX);
    let mut out: KVec<u64> = KVec::new();
    for i in 0..page_count as usize {
        let pa = entry.pages[base + i];
        out.push(pa.as_u64()).map_err(|_| FileMapError::NoMemory)?;
    }
    Ok(out)
}

/// Add `pages` mapping references; `false` if the handle is stale.
///
/// A *writable* mapping arms the writeback: a user store sets the CPU's PTE
/// dirty bit and nothing in this kernel harvests it, so every page of such a
/// set must be assumed written. Arming on a read-only mapping would rewrite an
/// unmodified file, stamping its timestamps and un-attesting its blocks.
pub fn retain(map: FileMapRef, pages: u32, writable: bool) -> bool {
    let mut sets = FILEMAP.lock();
    let Some(entry) = resolve(sets.as_mut_slice(), map) else {
        return false;
    };
    entry.refs = entry.refs.saturating_add(pages);
    if writable {
        entry.dirtyable = true;
    }
    revive(entry);
    true
}

/// Drop `pages` mapping references, queueing the writeback and the frame frees
/// when the last one goes.
///
/// Must not block, allocate or reach the filesystem: it is reached from process
/// teardown, under a preempt guard. A forgotten set owes no writeback, so its
/// frames go back here rather than through the queue.
pub fn release(map: FileMapRef, pages: u32) {
    let mut sets = FILEMAP.lock();
    let Some(entry) = resolve(sets.as_mut_slice(), map) else {
        return;
    };
    entry.refs = entry.refs.saturating_sub(pages);
    if entry.refs != 0 {
        return;
    }
    if entry.forgotten {
        drop_set(entry);
        return;
    }
    if !entry.pending_release {
        if entry.pending_flush {
            entry.pending_flush = false;
        } else {
            PENDING.fetch_add(1, Ordering::Relaxed);
        }
        entry.pending_release = true;
    }
}

/// Flush an inode's pages and then unkey the set, for a name that is about to
/// be removed. Must run **before** the removal, while the inode's blocks are
/// still its own, and with no filesystem lock held.
pub fn detach_inode(fs: &'static dyn FileSystem, inode: InodeId) {
    let _ = flush_inode(fs, inode);
    forget_inode(fs, inode);
}

/// Take the set for `(fs, inode)` out of lookup and out of writeback.
///
/// Not a free: a live mapping keeps reading the frames, which go back when the
/// last mapping does. What ends here is the *identity* — the inode number may
/// be reallocated to another file the moment its name is gone.
pub fn forget_inode(fs: &'static dyn FileSystem, inode: InodeId) {
    let mut sets = FILEMAP.lock();
    let Some(entry) = sets.iter_mut().find(|e| e.holds(fs, inode)) else {
        return;
    };
    if entry.pending_release || entry.pending_flush {
        entry.pending_release = false;
        entry.pending_flush = false;
        PENDING.fetch_sub(1, Ordering::Relaxed);
    }
    entry.forgotten = true;
    entry.dirtyable = false;
    if entry.refs == 0 {
        drop_set(entry);
    }
}

/// Free a set's frames and retire its slot. The generation bump is what makes
/// every outstanding [`FileMapRef`] for it resolve to a miss.
fn drop_set(entry: &mut PageSet) {
    // `PENDING` counts the sets carrying a flag, so clearing one here without
    // the matching decrement leaves the flusher's park predicate true forever.
    if entry.pending_release || entry.pending_flush {
        PENDING.fetch_sub(1, Ordering::Relaxed);
    }
    for pa in entry.pages.iter() {
        // The set's own MetaSlot ref, claimed in `claim_page`; with no mapping
        // left this is the last, so the frame returns to the buddy.
        release_owned_anon_page(*pa);
    }
    entry.pages = KVec::new();
    entry.fs = None;
    entry.ready = false;
    entry.dirtyable = false;
    entry.pending_release = false;
    entry.pending_flush = false;
    entry.forgotten = false;
    entry.generation = entry.generation.wrapping_add(1);
}

/// Write one set's pages back and wait for the filesystem to take them.
///
/// A live set that owes nothing — a forgotten one, whose pages are
/// deliberately not written back — answers `Ok`; only a handle naming no set
/// at all is [`FileMapError::Stale`].
pub fn flush(map: FileMapRef) -> Result<(), FileMapError> {
    let _io = io_lock()?;
    if !handle_is_live(map) {
        return Err(FileMapError::Stale);
    }
    let Some(job) = take_job_by_ref(map) else {
        return Ok(());
    };
    write_back(&job)
}

/// Does `map` still name a set, forgotten or not?
fn handle_is_live(map: FileMapRef) -> bool {
    let sets = FILEMAP.lock();
    sets.get(map.slot as usize)
        .is_some_and(|e| e.fs.is_some() && e.generation == map.generation)
}

/// [`flush`] for every set naming `inode`, for `fsync`/`sync`.
pub fn flush_inode(fs: &'static dyn FileSystem, inode: InodeId) -> Result<(), FileMapError> {
    let _io = io_lock()?;
    let Some(job) = take_job_by_inode(fs, inode) else {
        return Ok(());
    };
    write_back(&job)
}

/// Queue a writeback for `msync(MS_ASYNC)`; `false` if the handle names no set.
///
/// A forgotten set takes no queue entry: nothing would ever pick it up, and
/// the flusher's park predicate reads exactly that count.
pub fn queue_flush(map: FileMapRef) -> bool {
    let mut sets = FILEMAP.lock();
    let Some(entry) = resolve(sets.as_mut_slice(), map) else {
        return false;
    };
    if entry.forgotten {
        return true;
    }
    if !entry.pending_flush && !entry.pending_release {
        entry.pending_flush = true;
        PENDING.fetch_add(1, Ordering::Relaxed);
    }
    true
}

/// Sets owing writeback. Read by a writeback thread's wait predicate, which
/// must take no lock.
pub fn pending_count() -> usize {
    PENDING.load(Ordering::Relaxed)
}

/// One set's pages, lifted out from under the bookkeeping lock so the
/// writeback runs without it.
struct WriteJob {
    slot: usize,
    generation: u32,
    fs: &'static dyn FileSystem,
    inode: InodeId,
    first_page: u64,
    pages: KVec<PhysAddr>,
    dirtyable: bool,
    release: bool,
}

/// Snapshot the set in `slot` for writeback, clearing whatever queue entry it
/// had — a set whose obligation is dropped here re-queues on its next
/// [`release`], so the loop in [`drain_pending`] always terminates.
///
/// A forgotten set is never a job: the blocks its pages came from may already
/// belong to something else.
fn take_job_slot(slot: usize) -> Option<WriteJob> {
    let mut sets = FILEMAP.lock();
    let entry = sets.get_mut(slot)?;
    let fs = entry.fs?;
    if entry.forgotten {
        return None;
    }
    // Staged **before** the obligation is cleared: a failure after the clear
    // would leave a set with no queue entry and nobody to free it.
    let mut pages: KVec<PhysAddr> = KVec::new();
    if pages.try_reserve_exact(entry.pages.len()).is_err() {
        return None;
    }
    for pa in entry.pages.iter() {
        if pages.push(*pa).is_err() {
            return None;
        }
    }
    if entry.pending_release || entry.pending_flush {
        PENDING.fetch_sub(1, Ordering::Relaxed);
    }
    let release = entry.pending_release && entry.refs == 0;
    entry.pending_flush = false;
    entry.pending_release = false;
    Some(WriteJob {
        slot,
        generation: entry.generation,
        fs,
        inode: entry.inode,
        first_page: entry.first_page,
        pages,
        dirtyable: entry.dirtyable && entry.ready,
        release,
    })
}

/// The set `map` names, if it is still live.
fn take_job_by_ref(map: FileMapRef) -> Option<WriteJob> {
    {
        let sets = FILEMAP.lock();
        let entry = sets.get(map.slot as usize)?;
        if entry.fs.is_none() || entry.generation != map.generation {
            return None;
        }
    }
    take_job_slot(map.slot as usize)
}

/// The set holding `(fs, inode)`, if any.
fn take_job_by_inode(fs: &'static dyn FileSystem, inode: InodeId) -> Option<WriteJob> {
    let slot = {
        let sets = FILEMAP.lock();
        sets.iter().position(|e| e.holds(fs, inode))?
    };
    take_job_slot(slot)
}

/// The next set owing writeback.
fn take_queued_job() -> Option<WriteJob> {
    let slot = {
        let sets = FILEMAP.lock();
        sets.iter()
            .position(|e| (e.pending_release || e.pending_flush) && !e.forgotten)?
    };
    take_job_slot(slot)
}

/// Write a job's pages out, then complete a queued release.
fn write_back(job: &WriteJob) -> Result<(), FileMapError> {
    let result = if job.dirtyable {
        write_pages(job)
    } else {
        Ok(())
    };
    if job.release {
        finish_release(job);
    }
    result
}

/// The device-facing half, clamped to the file's current size: the last page of
/// a mapping may extend past EOF, and writing all of it would grow the file by
/// whatever the zero-fill put there.
#[inline(never)]
fn write_pages(job: &WriteJob) -> Result<(), FileMapError> {
    let size = job.fs.stat(job.inode).map_err(|_| FileMapError::Io)?.size;
    let mut staging = KVec::<u8>::zeroed(PAGE_SIZE_USIZE).map_err(|_| FileMapError::NoMemory)?;
    for (i, pa) in job.pages.iter().enumerate() {
        let offset = (job.first_page + i as u64) * PAGE_SIZE;
        if offset >= size {
            break;
        }
        let len = usize::try_from((size - offset).min(PAGE_SIZE)).unwrap_or(PAGE_SIZE_USIZE);
        let virt = pa.try_to_virt().ok_or(FileMapError::Io)?;
        if !slopos_ostd::mm::hhdm_bytes::read_bytes(virt, 0, &mut staging.as_mut_slice()[..len]) {
            return Err(FileMapError::Io);
        }
        let mut done = 0usize;
        while done < len {
            match job.fs.write(
                job.inode,
                offset + done as u64,
                &staging.as_slice()[done..len],
            ) {
                Ok(0) => return Err(FileMapError::Io),
                Ok(n) => done += n,
                Err(_) => return Err(FileMapError::Io),
            }
        }
    }
    Ok(())
}

/// Free the frames and the slot, once the writeback has gone out.
fn finish_release(job: &WriteJob) {
    let mut sets = FILEMAP.lock();
    let entry = &mut sets[job.slot];
    if entry.generation != job.generation || entry.refs != 0 || entry.fs.is_none() {
        return;
    }
    drop_set(entry);
}

/// Complete every queued writeback and frame free.
///
/// Blocks and reaches the filesystem, so the caller must be able to sleep and
/// hold no filesystem lock.
pub fn drain_pending() {
    if PENDING.load(Ordering::Relaxed) == 0 {
        return;
    }
    let Ok(_io) = io_lock() else {
        return;
    };
    while let Some(job) = take_queued_job() {
        run_job(&job);
    }
}

/// Write back every set, queued or live, and complete the queued frees — the
/// scope `sync(2)` and shutdown need, where a mapped page that never reached
/// the filesystem would be lost while the image was marked clean.
pub fn flush_all() {
    let Ok(_io) = io_lock() else {
        return;
    };
    for slot in 0..MAX_MAPPED_INODES {
        if let Some(job) = take_job_slot(slot) {
            run_job(&job);
        }
    }
}

/// The bytes are lost either way on a failure; re-queueing would spin against
/// a filesystem that is refusing writes.
fn run_job(job: &WriteJob) {
    if let Err(e) = write_back(job) {
        klog_info!("filemap: writeback of inode {} failed: {:?}", job.inode, e);
    }
}

/// What a ready page set holds around a file offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// The set holds this offset; it is the authority for the bytes there.
    Here,
    /// The set's coverage begins at this higher offset, so a chunk starting
    /// below must be cut there rather than run into it.
    Above(u64),
    /// Nothing here or above: the filesystem answers the whole chunk.
    Absent,
}

/// Where the page set for `(fs, inode)` stands relative to `offset`.
///
/// One lookup for both questions the `read(2)`/`write(2)` hooks ask, on the
/// path every regular-file chunk takes: an unmapped file pays one bounded scan
/// under a spinlock and no filesystem call.
pub fn coverage_at(fs: &'static dyn FileSystem, inode: InodeId, offset: u64) -> Coverage {
    let sets = FILEMAP.lock();
    let Some(entry) = sets.iter().find(|e| e.holds(fs, inode)) else {
        return Coverage::Absent;
    };
    if !entry.ready || entry.pages.is_empty() {
        return Coverage::Absent;
    }
    if page_cursor(entry, offset).is_some() {
        return Coverage::Here;
    }
    let start = entry.first_page * PAGE_SIZE;
    if start > offset {
        Coverage::Above(start)
    } else {
        Coverage::Absent
    }
}

/// Does the set hold `offset` itself?
pub fn covers_offset(fs: &'static dyn FileSystem, inode: InodeId, offset: u64) -> bool {
    coverage_at(fs, inode, offset) == Coverage::Here
}

/// Serve a `read(2)` from the page set, when it covers `offset`.
///
/// `None` means the caller must read the filesystem instead. A short answer is
/// the end of the covered range, not EOF. `buf` must already be clipped to the
/// file's length: the set holds whole pages, whose tail past EOF is zero-fill.
pub fn read_through(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    offset: u64,
    buf: &mut [u8],
) -> Option<usize> {
    if buf.is_empty() {
        return None;
    }
    let sets = FILEMAP.lock();
    let entry = sets.iter().find(|e| e.holds(fs, inode))?;
    if !entry.ready {
        return None;
    }
    let (mut idx, mut page_off) = page_cursor(entry, offset)?;
    let mut done = 0usize;
    while done < buf.len() && idx < entry.pages.len() {
        let take = (PAGE_SIZE_USIZE - page_off).min(buf.len() - done);
        let virt = entry.pages[idx].try_to_virt()?;
        if !slopos_ostd::mm::hhdm_bytes::read_bytes(virt, page_off, &mut buf[done..done + take]) {
            break;
        }
        done += take;
        idx += 1;
        page_off = 0;
    }
    (done > 0).then_some(done)
}

/// What [`write_through`] did with a `write(2)` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteThrough {
    /// Bytes written into the page set; the caller advances by this many.
    Served(usize),
    /// No page set covers this offset — the caller writes the filesystem.
    NotCovered,
    /// The caller was killed while waiting for the writeback mutex. The
    /// filesystem must *not* be written behind the set's back: bytes landing
    /// there would leave every mapper reading stale pages.
    Interrupted,
}

/// Apply a `write(2)` to the page set, when it covers `offset`.
///
/// The bytes are not passed to the filesystem here: the set is the authority
/// for the pages it holds, and its writeback is what puts them on the device.
pub fn write_through(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    offset: u64,
    buf: &[u8],
) -> WriteThrough {
    if buf.is_empty() {
        return WriteThrough::NotCovered;
    }
    // Ordered against an in-flight populate: without it a write landing
    // mid-populate would be read back over by the pages the populate installs.
    let Ok(_io) = io_lock() else {
        return WriteThrough::Interrupted;
    };
    let mut sets = FILEMAP.lock();
    let Some(entry) = sets.iter_mut().find(|e| e.holds(fs, inode)) else {
        return WriteThrough::NotCovered;
    };
    if !entry.ready {
        return WriteThrough::NotCovered;
    }
    let Some((mut idx, mut page_off)) = page_cursor(entry, offset) else {
        return WriteThrough::NotCovered;
    };
    let mut done = 0usize;
    while done < buf.len() && idx < entry.pages.len() {
        let take = (PAGE_SIZE_USIZE - page_off).min(buf.len() - done);
        let Some(virt) = entry.pages[idx].try_to_virt() else {
            break;
        };
        if !slopos_ostd::mm::hhdm_bytes::write_bytes(virt, page_off, &buf[done..done + take]) {
            break;
        }
        done += take;
        idx += 1;
        page_off = 0;
    }
    if done == 0 {
        return WriteThrough::NotCovered;
    }
    entry.dirtyable = true;
    WriteThrough::Served(done)
}

/// `(index into `pages`, byte offset in that page)` for a file offset the set
/// covers.
fn page_cursor(entry: &PageSet, offset: u64) -> Option<(usize, usize)> {
    let page = offset / PAGE_SIZE;
    if page < entry.first_page {
        return None;
    }
    let idx = usize::try_from(page - entry.first_page).ok()?;
    if idx >= entry.pages.len() {
        return None;
    }
    Some((idx, (offset % PAGE_SIZE) as usize))
}

struct FileMapHook;

static FILEMAP_HOOK: FileMapHook = FileMapHook;

impl FileMapOps for FileMapHook {
    fn retain(&self, map: FileMapRef, pages: u32, writable: bool) -> bool {
        retain(map, pages, writable)
    }

    fn release(&self, map: FileMapRef, pages: u32) {
        release(map, pages);
    }

    fn drain(&self) {
        drain_pending();
    }
}

/// The registry, for `mm` to call on unmap, fork and teardown.
pub fn filemap_ops() -> &'static dyn FileMapOps {
    &FILEMAP_HOOK
}

/// Page sets currently held, for tests and diagnostics.
pub fn mapped_inode_count() -> usize {
    FILEMAP.lock().iter().filter(|e| e.fs.is_some()).count()
}
