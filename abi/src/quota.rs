//! Resource-accounting vocabulary: the kinds, their units, and the errno each
//! refusal is reported as.
//!
//! Pure data, and deliberately in `abi` rather than in `slopos-ostd`. This
//! crate carries no `#![feature(...)]` and is depended on by userland-side
//! crates, so nothing here may need a nightly gate. The mechanism — the arena,
//! the debit walk, the linear `Charge` token — lives in `slopos-ostd`, where
//! `check_safe_contract_surface.sh` can see it.
//!
//! [`ResourceKind`] is the runtime name of an axis; the marker types below are
//! its compile-time name. Both exist because the arena indexes rows by a
//! number while the token is parameterised by a type: an enum const-generic
//! parameter would need `adt_const_params` and `#[derive(ConstParamTy)]`,
//! which is exactly the feature gate this crate may not carry.

use crate::errno::Errno;

/// What is being counted.
///
/// The discriminant is the row index into an account's per-kind arrays, so the
/// values are contiguous from zero and [`KIND_COUNT`] is one past the last.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResourceKind {
    /// A descriptor number in a table. Charged to the table's holder.
    FdSlot = 0,
    /// A registry row plus its backing object. Charged to the creator, once.
    ObjectRow = 1,
    /// A schedulable task.
    Task = 2,
    /// An address space with its own identity.
    Process = 3,
    /// Pages of mapped or reserved memory.
    Pages = 4,
    /// Bytes pinned against reclaim, for DMA or a registered buffer.
    PinnedBytes = 5,
    /// An alias held by kernel state owned by neither party — in-flight
    /// `SCM_RIGHTS`, a ring's in-flight file reference.
    Custody = 6,
    /// Kernel-internal memory attributable to a principal: page tables, task
    /// stacks, slab backing.
    KernelMeta = 7,
}

/// Number of distinct [`ResourceKind`]s. The width of every per-kind array in
/// an account row.
pub const KIND_COUNT: usize = 8;

impl ResourceKind {
    /// Every kind, in discriminant order. The iteration order of every dump
    /// and every audit.
    pub const ALL: [ResourceKind; KIND_COUNT] = [
        ResourceKind::FdSlot,
        ResourceKind::ObjectRow,
        ResourceKind::Task,
        ResourceKind::Process,
        ResourceKind::Pages,
        ResourceKind::PinnedBytes,
        ResourceKind::Custody,
        ResourceKind::KernelMeta,
    ];

    /// Row index into an account's per-kind arrays.
    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Short, stable name. Read by the `ledger` diagnostic command and by the
    /// headroom gate's line parser, so it is part of that gate's contract.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            ResourceKind::FdSlot => "fdslot",
            ResourceKind::ObjectRow => "objectrow",
            ResourceKind::Task => "task",
            ResourceKind::Process => "process",
            ResourceKind::Pages => "pages",
            ResourceKind::PinnedBytes => "pinnedbytes",
            ResourceKind::Custody => "custody",
            ResourceKind::KernelMeta => "kernelmeta",
        }
    }

    /// What one unit of this kind measures.
    #[inline]
    pub const fn unit(self) -> Unit {
        match self {
            ResourceKind::FdSlot
            | ResourceKind::ObjectRow
            | ResourceKind::Task
            | ResourceKind::Process
            | ResourceKind::Custody => Unit::Count,
            // `PinnedBytes` is named for the resource, not the unit: the
            // charge is in **pages**. `MAX_PIN_BYTES` is 1 GiB, which does not
            // fit the arena's `u32` amount, and pages are what a pin actually
            // holds against reclaim — a byte count would also let a thousand
            // sub-page pins look cheap while each holds a whole frame.
            ResourceKind::Pages | ResourceKind::KernelMeta | ResourceKind::PinnedBytes => {
                Unit::Pages
            }
        }
    }

    /// When the charge is given back.
    #[inline]
    pub const fn refund(self) -> Refund {
        match self {
            // A `Task`'s destruction is deferred to the graveyard, so a
            // `Drop`-refund would keep a thousand exited tasks charged until
            // the drain — spurious `EAGAIN` on fork under exactly the load the
            // quota exists to bound. The refund happens at the exit latch
            // instead, beside the `num_tasks` decrement.
            ResourceKind::Task => Refund::OnExitLatch,
            _ => Refund::OnDrop,
        }
    }

    /// The errno a refused charge of this kind is reported as.
    ///
    /// Every one of these is already what the corresponding call site
    /// returns on its own capacity failure, so enforcement mints no new code
    /// at the ABI boundary.
    #[inline]
    pub const fn errno(self) -> Errno {
        match self {
            // Per-process descriptor table full.
            ResourceKind::FdSlot => Errno::EMFILE,
            // System-wide object table full.
            ResourceKind::ObjectRow | ResourceKind::Custody => Errno::ENFILE,
            // fork/clone refusal.
            ResourceKind::Task | ResourceKind::Process => Errno::EAGAIN,
            ResourceKind::Pages | ResourceKind::PinnedBytes | ResourceKind::KernelMeta => {
                Errno::ENOMEM
            }
        }
    }
}

/// What one unit of a kind measures. Display only — the arena counts `u32`s
/// whatever the unit is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    Count,
    Bytes,
    Pages,
}

impl Unit {
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            Unit::Count => "count",
            Unit::Bytes => "bytes",
            Unit::Pages => "pages",
        }
    }
}

/// When a charge of a given kind is given back. A property of the kind, never
/// of the call site: two sites refunding one kind differently is how a ledger
/// starts disagreeing with itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refund {
    /// The token's `Drop` refunds.
    OnDrop,
    /// The refund is applied at the task exit latch, because the object's own
    /// destruction is deferred past the point the resource is actually free.
    OnExitLatch,
}

/// The enforced per-process default for this kind, or [`NO_LIMIT_SENTINEL`]
/// where no ceiling is enforced yet.
///
/// **Measured, never chosen** — but deliberately a *different number* from the
/// gate ceiling in `scripts/gates/quota/<variant>.txt`, and in a different
/// place. Deriving the enforced default from a boot-time observation is how
/// Linux shipped limits that could not subsequently be raised; deriving the
/// gate ceiling from the enforced default would make the ratchet measure its
/// own configuration. Two numbers, two homes, on purpose.
///
/// A kind whose charge sites have not landed yet answers [`NO_LIMIT_SENTINEL`]
/// rather than a guess. A ceiling on a kind nothing charges bounds nothing and
/// would be a number with no reader.
#[inline]
pub const fn default_process_limit(kind: ResourceKind) -> u32 {
    match kind {
        // The per-process descriptor table is 256 entries, so this is the
        // existing array bound restated as a per-principal ceiling. The worst
        // single process measured across a full test boot plus the
        // session-smoke population was 18, so there is an order of magnitude
        // of headroom before this binds — which is the point: it bounds the
        // adversary, not the workload.
        ResourceKind::FdSlot => 256,
        // Strictly above the descriptor ceiling, and that relation is
        // load-bearing rather than slack.
        //
        // A process legitimately holds objects that no descriptor names — an
        // in-flight `SCM_RIGHTS` reference, a ring's in-flight file reference
        // — so the two counts are not the same quantity and the object bound
        // must not be the tighter one. Set equal, `ObjectRow` becomes the
        // *de-facto* descriptor limit: an `open` charges the object before the
        // descriptor number, so a full table would refuse with `ENFILE`
        // ("system-wide table full") where POSIX requires `EMFILE` ("this
        // process holds too many descriptors"), and userland reading that
        // errno would back off against the wrong resource.
        //
        // Measured worst single process: 10, against 18 descriptors.
        ResourceKind::ObjectRow => 512,
        // Threads per process. `MAX_TASKS` is 8192 global, so without a
        // per-principal bound one process spends the whole table; 512 is well
        // above anything in this tree (the busiest measured process holds a
        // handful) and far below the global ceiling.
        ResourceKind::Task => 512,
        // `MAX_PROCESSES` is 256 and is reached long before `MAX_TASKS`, so
        // this is the tighter global resource. A principal may spawn a
        // quarter of the table before it is refused.
        ResourceKind::Process => 64,
        // In-flight `SCM_RIGHTS` references, held by no descriptor table. The
        // structural bound is 8 fds x 2 directions x 16 pairs = 256 across
        // the whole system; per principal this is the tighter of the two.
        ResourceKind::Custody => 64,
        // Pinned pages, charged in pages rather than bytes. 16 MiB of pinned
        // memory per principal: enough for the ring buffer sets this tree
        // registers, bounded against a process pinning until the machine
        // cannot reclaim.
        ResourceKind::PinnedBytes => 4096,
        // Kernel memory attributable to a principal: today its task stacks,
        // at 12 pages each (32 KiB kernel + 16 KiB data). 2048 pages is 8 MiB
        // per principal, or roughly 170 threads' worth of stack — comfortably
        // above the `Task` ceiling of 512 threads only because most processes
        // are single-threaded, which is deliberate: this bounds the *memory*,
        // not the thread count, and the two ceilings bind independently.
        ResourceKind::KernelMeta => 2048,
        // Mapped **virtual** pages per address space -- `RLIMIT_AS`, not RSS.
        // 256 MiB of address space per principal.
        //
        // Deliberately a VA bound and not a resident one. An RSS-shaped
        // resource needs a reclaim disposition for the case where a process is
        // already over it, which is a kill daemon and an exception taxonomy in
        // XNU and which illumos declined to put in its synchronous framework
        // at all; a VA bound is refusable at the syscall that asks for it,
        // which is the only place a refusal has an errno to travel back on.
        //
        // Measured worst single address space across a full test boot plus the
        // session-smoke population: 30 998 pages (121 MiB), which is a test
        // binary's own mappings rather than anything adversarial. 65 536 is a
        // shade over twice that -- deliberately tighter in *multiples* than
        // the other kinds, because the demand-paging split means a process can
        // map far more than it will ever populate, so the headroom this needs
        // is bounded by what a real workload maps rather than by what it
        // touches.
        ResourceKind::Pages => 65536,
    }
}

/// "No ceiling." Distinct from a limit of zero, which refuses everything.
pub const NO_LIMIT_SENTINEL: u32 = u32::MAX;

/// How a refused charge is answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QuotaMode {
    /// No ceiling is consulted. Counters still move, so the ledger still
    /// measures.
    Off,
    /// An over-limit charge is counted as a denial and *granted*. The tier the
    /// peaks are measured on: a system that dies at the first over-limit
    /// cannot report what its real peak would have been.
    Warn,
    /// An over-limit charge is refused with the kind's errno.
    Enforce,
}

// ---------------------------------------------------------------------------
// The `prlimit64` view
// ---------------------------------------------------------------------------

// Linux's `RLIMIT_*` numbering and `struct rlimit64` layout, from
// `asm-generic/resource.h`. Interface facts: ABI numbers and struct layouts
// carry no copyright, which is what makes the compatibility work sound.

/// `struct rlimit64`. Soft limit, then hard limit.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RLimit64 {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

/// "No limit", as `prlimit64` spells it.
pub const RLIM64_INFINITY: u64 = u64::MAX;

pub const RLIMIT_DATA: u32 = 2;
pub const RLIMIT_NPROC: u32 = 6;
pub const RLIMIT_NOFILE: u32 = 7;
pub const RLIMIT_MEMLOCK: u32 = 8;
pub const RLIMIT_AS: u32 = 9;

/// How a `RLIMIT_*` maps onto a [`ResourceKind`], and what one unit of the
/// limit is worth in that kind's units.
///
/// The scale is what stops the two vocabularies disagreeing: `RLIMIT_AS` and
/// `RLIMIT_MEMLOCK` are byte-denominated in the ABI while the arena counts
/// pages, so publishing a page count under a byte-named limit would understate
/// the bound by a factor of 4096 and a caller sizing an allocation against it
/// would back off far too early.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RLimitMapping {
    pub kind: ResourceKind,
    /// Bytes (or items) per unit the arena counts.
    pub scale: u64,
}

/// The [`ResourceKind`] a `RLIMIT_*` names, or `None` for one this kernel does
/// not enforce.
///
/// Returning `None` rather than a plausible-looking infinity is the whole
/// point: a kernel that reports `RLIM64_INFINITY` for a limit it does not
/// enforce actively defeats userland self-limiting, because a caller cannot
/// distinguish "unbounded" from "unimplemented". An unmapped resource is
/// `EINVAL`, which a caller can act on.
#[inline]
pub const fn rlimit_mapping(resource: u32) -> Option<RLimitMapping> {
    match resource {
        RLIMIT_NOFILE => Some(RLimitMapping {
            kind: ResourceKind::FdSlot,
            scale: 1,
        }),
        RLIMIT_NPROC => Some(RLimitMapping {
            kind: ResourceKind::Process,
            scale: 1,
        }),
        // Byte-denominated in the ABI; the arena counts pages.
        RLIMIT_AS | RLIMIT_DATA => Some(RLimitMapping {
            kind: ResourceKind::Pages,
            scale: 4096,
        }),
        RLIMIT_MEMLOCK => Some(RLimitMapping {
            kind: ResourceKind::PinnedBytes,
            scale: 4096,
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Axis marker types
// ---------------------------------------------------------------------------

// One zero-sized type per kind. `slopos-ostd` seals them and hangs the axis
// trait off each, which buys an associated cost, an associated amount type and
// per-axis impls that an enum const-generic parameter could not — for no
// feature gate.

macro_rules! axes {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {
        $(
            $(#[$doc])*
            #[derive(Clone, Copy, PartialEq, Eq, Debug)]
            pub struct $name;
        )*
    };
}

axes! {
    /// [`ResourceKind::FdSlot`].
    FdSlot,
    /// [`ResourceKind::ObjectRow`].
    ObjectRow,
    /// [`ResourceKind::Task`].
    TaskCount,
    /// [`ResourceKind::Process`].
    ProcCount,
    /// [`ResourceKind::Pages`].
    PagesAxis,
    /// [`ResourceKind::PinnedBytes`].
    PinnedBytesAxis,
    /// [`ResourceKind::Custody`].
    CustodyAxis,
    /// [`ResourceKind::KernelMeta`].
    KernelMetaAxis,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_indices_are_dense_and_ordered() {
        for (i, kind) in ResourceKind::ALL.iter().enumerate() {
            assert_eq!(kind.index(), i, "{kind:?} is out of discriminant order");
        }
        assert_eq!(ResourceKind::ALL.len(), KIND_COUNT);
    }

    #[test]
    fn every_kind_has_a_distinct_name() {
        for (i, a) in ResourceKind::ALL.iter().enumerate() {
            for b in &ResourceKind::ALL[i + 1..] {
                assert_ne!(a.name(), b.name(), "{a:?} and {b:?} share a name");
            }
            assert!(!a.name().is_empty());
        }
    }

    /// The errno mapping is what makes enforcement invisible at the ABI: each
    /// refusal has to be the code that call site already returns.
    #[test]
    fn errno_mapping_matches_the_call_sites() {
        assert_eq!(ResourceKind::FdSlot.errno(), Errno::EMFILE);
        assert_eq!(ResourceKind::ObjectRow.errno(), Errno::ENFILE);
        assert_eq!(ResourceKind::Custody.errno(), Errno::ENFILE);
        assert_eq!(ResourceKind::Task.errno(), Errno::EAGAIN);
        assert_eq!(ResourceKind::Process.errno(), Errno::EAGAIN);
        assert_eq!(ResourceKind::Pages.errno(), Errno::ENOMEM);
        assert_eq!(ResourceKind::KernelMeta.errno(), Errno::ENOMEM);
    }

    /// The object ceiling must sit strictly above the descriptor ceiling.
    ///
    /// An `open` charges the object row before the descriptor number, so if
    /// the object bound were the tighter of the two it would refuse first and
    /// a process at its descriptor limit would see `ENFILE` where POSIX
    /// requires `EMFILE`. A process also legitimately holds objects no
    /// descriptor names, which is the substantive reason the two counts differ.
    #[test]
    fn the_object_ceiling_clears_the_descriptor_ceiling() {
        let fds = default_process_limit(ResourceKind::FdSlot);
        let objects = default_process_limit(ResourceKind::ObjectRow);
        assert!(
            objects > fds,
            "ObjectRow ({objects}) must exceed FdSlot ({fds}), or it silently \
             becomes the descriptor limit and reports the wrong errno"
        );
    }

    /// Every kind is charged somewhere and therefore carries a ceiling.
    ///
    /// Stated over the whole set rather than over a list of the wired ones, so
    /// a kind added later without charge sites fails here instead of shipping
    /// a limit that refuses against a counter nothing increments -- or, worse,
    /// a `NO_LIMIT_SENTINEL` nobody notices is unbounded.
    #[test]
    fn every_kind_carries_a_ceiling() {
        for kind in ResourceKind::ALL {
            assert_ne!(
                default_process_limit(kind),
                NO_LIMIT_SENTINEL,
                "{kind:?} has no enforced ceiling: either wire its charge sites \
                 or say here why it is unbounded"
            );
        }
    }

    /// Every published `RLIMIT_*` names a kind that is actually enforced.
    ///
    /// The failure this prevents is the one Redox and Asterinas ship: a limit
    /// reported to userland that nothing consults. A caller that cannot query
    /// a real bound cannot back off gracefully, and one that queries a fake
    /// one backs off against nothing.
    #[test]
    fn every_published_rlimit_maps_to_an_enforced_kind() {
        for resource in [
            RLIMIT_DATA,
            RLIMIT_NPROC,
            RLIMIT_NOFILE,
            RLIMIT_MEMLOCK,
            RLIMIT_AS,
        ] {
            let mapping = rlimit_mapping(resource).expect("published limits must map");
            assert_ne!(
                default_process_limit(mapping.kind),
                NO_LIMIT_SENTINEL,
                "RLIMIT {resource} maps to {:?}, which enforces nothing",
                mapping.kind
            );
            assert!(mapping.scale > 0);
        }
    }

    /// An unknown resource is unmapped rather than silently infinite.
    #[test]
    fn unknown_rlimits_are_unmapped() {
        for resource in [0u32, 1, 3, 4, 5, 10, 42] {
            assert!(rlimit_mapping(resource).is_none(), "{resource}");
        }
    }

    /// A byte-denominated limit scales by the page size the arena counts in.
    #[test]
    fn byte_denominated_limits_carry_the_page_scale() {
        for resource in [RLIMIT_AS, RLIMIT_DATA, RLIMIT_MEMLOCK] {
            assert_eq!(rlimit_mapping(resource).unwrap().scale, 4096, "{resource}");
        }
        assert_eq!(rlimit_mapping(RLIMIT_NOFILE).unwrap().scale, 1);
    }

    #[test]
    fn only_task_refunds_at_the_exit_latch() {
        for kind in ResourceKind::ALL {
            let want = if kind == ResourceKind::Task {
                Refund::OnExitLatch
            } else {
                Refund::OnDrop
            };
            assert_eq!(kind.refund(), want, "{kind:?}");
        }
    }
}
