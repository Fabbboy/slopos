//! Resource-accounting vocabulary: the kinds, their units, and the errno each
//! refusal is reported as. The mechanism lives in `slopos-ostd`; the vocabulary
//! is here because this crate carries no `#![feature(...)]` and userland links it.

use crate::errno::Errno;

/// What is being counted. The discriminant is the row index into an account's
/// per-kind arrays, so the values are contiguous from zero and [`KIND_COUNT`]
/// is one past the last.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResourceKind {
    /// A descriptor number in a table. Charged to the table's holder.
    FdSlot = 0,
    /// A registry row plus its backing object. Charged to the creator, once.
    ObjectRow = 1,
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

/// Width of every per-kind array in an account row.
pub const KIND_COUNT: usize = 8;

impl ResourceKind {
    /// Discriminant order — the iteration order of every dump and every audit.
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

    /// Short, stable name. The headroom gate's line parser reads it, so it is
    /// part of that gate's contract.
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

    #[inline]
    pub const fn unit(self) -> Unit {
        match self {
            ResourceKind::FdSlot
            | ResourceKind::ObjectRow
            | ResourceKind::Task
            | ResourceKind::Process
            | ResourceKind::Custody => Unit::Count,
            // `PinnedBytes` charges in pages: a byte count overflows the arena's
            // `u32`, and a sub-page pin still holds a whole frame against reclaim.
            ResourceKind::Pages | ResourceKind::KernelMeta | ResourceKind::PinnedBytes => {
                Unit::Pages
            }
        }
    }

    #[inline]
    pub const fn refund(self) -> Refund {
        match self {
            // A `Task`'s destruction is deferred to the graveyard, so a
            // `Drop`-refund would keep exited tasks charged until the drain.
            ResourceKind::Task => Refund::OnExitLatch,
            _ => Refund::OnDrop,
        }
    }

    /// The errno a refused charge is reported as — always the code the call site
    /// already returns on its own capacity failure, so enforcement mints no new
    /// code at the ABI boundary.
    #[inline]
    pub const fn errno(self) -> Errno {
        match self {
            ResourceKind::FdSlot => Errno::EMFILE,
            ResourceKind::ObjectRow | ResourceKind::Custody => Errno::ENFILE,
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
/// of the call site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refund {
    OnDrop,
    /// Applied at the task exit latch: the object's own destruction is deferred
    /// past the point the resource is actually free.
    OnExitLatch,
}

/// The enforced per-process default for this kind, or [`NO_LIMIT_SENTINEL`]
/// where no ceiling is enforced yet.
///
/// Measured, never chosen — and deliberately a different number, in a different
/// file, from the gate ceiling in `scripts/gates/quota/<variant>.txt`, so the
/// ratchet never measures its own configuration.
#[inline]
pub const fn default_process_limit(kind: ResourceKind) -> u32 {
    match kind {
        // The per-process descriptor table is 256 entries; measured worst 18.
        ResourceKind::FdSlot => 256,
        // Must stay strictly above the descriptor ceiling: an `open` charges the
        // object row before the descriptor number, so an equal bound would refuse
        // with `ENFILE` where POSIX requires `EMFILE`. Measured worst 10.
        ResourceKind::ObjectRow => 512,
        // Threads per process, against a global `MAX_TASKS` of 8192.
        ResourceKind::Task => 512,
        // `MAX_PROCESSES` is 256: a quarter of the table per principal.
        ResourceKind::Process => 64,
        // In-flight `SCM_RIGHTS` references, held by no descriptor table; the
        // structural system-wide bound is 8 fds x 2 directions x 16 pairs.
        ResourceKind::Custody => 64,
        // 16 MiB of pinned memory per principal, charged in pages.
        ResourceKind::PinnedBytes => 4096,
        // 8 MiB per principal — today its task stacks, at 12 pages each. Bounds
        // the memory, not the thread count, so it binds independently of `Task`.
        ResourceKind::KernelMeta => 2048,
        // Mapped *virtual* pages -- `RLIMIT_AS`, not RSS: a VA bound is refusable
        // at the syscall that asks for it, the only place a refusal has an errno
        // to travel back on, whereas an RSS bound needs a reclaim disposition for
        // a process already over it. 256 MiB, against a measured worst of 30 998.
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
    /// peaks are measured on: dying at the first over-limit hides the real peak.
    Warn,
    /// An over-limit charge is refused with the kind's errno.
    Enforce,
}

// `RLIMIT_*` numbering and `struct rlimit64` layout follow Linux's `asm-generic/resource.h`.

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

/// How a `RLIMIT_*` maps onto a [`ResourceKind`]. The scale reconciles the two
/// vocabularies: `RLIMIT_AS` and `RLIMIT_MEMLOCK` are byte-denominated in the
/// ABI while the arena counts pages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RLimitMapping {
    pub kind: ResourceKind,
    /// Bytes (or items) per unit the arena counts.
    pub scale: u64,
}

/// The [`ResourceKind`] a `RLIMIT_*` names, or `None` for one this kernel does
/// not enforce. `None` rather than a plausible-looking infinity: a caller cannot
/// distinguish "unbounded" from "unimplemented", so an unmapped resource is
/// `EINVAL`, which it can act on.
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

// One zero-sized type per kind. `slopos-ostd` seals them and hangs the axis trait
// off each, buying associated cost and amount types an enum const-generic
// parameter could not — with no feature gate.

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
    FdSlot,
    ObjectRow,
    TaskCount,
    ProcCount,
    PagesAxis,
    PinnedBytesAxis,
    CustodyAxis,
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

    #[test]
    fn unknown_rlimits_are_unmapped() {
        for resource in [0u32, 1, 3, 4, 5, 10, 42] {
            assert!(rlimit_mapping(resource).is_none(), "{resource}");
        }
    }

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
