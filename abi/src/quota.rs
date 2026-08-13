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
            ResourceKind::Pages | ResourceKind::KernelMeta => Unit::Pages,
            ResourceKind::PinnedBytes => Unit::Bytes,
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
