//! The axis trait: a resource kind as a type.
//!
//! Each marker type in [`slopos_abi::quota`] gets an impl carrying the row
//! index, the unit, the refund policy and the cost of one item. Cost belongs
//! to the axis rather than the caller so two call sites charging the same
//! resource cannot disagree about what one costs. The trait is sealed: an
//! axis claims the arena has a row for this kind, and only this crate can
//! make that claim true.

use slopos_abi::quota::{
    CustodyAxis, FdSlot, KernelMetaAxis, ObjectRow, PagesAxis, PinnedBytesAxis, ProcCount, Refund,
    ResourceKind, TaskCount, Unit,
};

mod sealed {
    pub trait Sealed {}
}

pub trait ResourceAxis: sealed::Sealed + Copy + 'static {
    const KIND: ResourceKind;
    const NAME: &'static str;
    const UNIT: Unit;
    const REFUND: Refund;
    const COST: u32;
}

/// An axis whose resource is a reservation, and therefore has a refund path.
///
/// A separate marker rather than a method on [`ResourceAxis`]: only a
/// `Refundable` axis can parameterise a [`Charge`](super::Charge), so an axis
/// for a *consumed* resource cannot accidentally acquire a refunding
/// destructor by implementing one trait.
pub trait Refundable: ResourceAxis {}

macro_rules! impl_axis {
    ($($ty:ty => $kind:ident, cost = $cost:expr;)*) => {
        $(
            impl sealed::Sealed for $ty {}
            impl ResourceAxis for $ty {
                const KIND: ResourceKind = ResourceKind::$kind;
                const NAME: &'static str = ResourceKind::$kind.name();
                const UNIT: Unit = ResourceKind::$kind.unit();
                const REFUND: Refund = ResourceKind::$kind.refund();
                const COST: u32 = $cost;
            }
            impl Refundable for $ty {}
        )*
    };
}

impl_axis! {
    FdSlot          => FdSlot,      cost = 1;
    ObjectRow       => ObjectRow,   cost = 1;
    TaskCount       => Task,        cost = 1;
    ProcCount       => Process,     cost = 1;
    CustodyAxis     => Custody,     cost = 1;
    // The amount is the page / byte count, so one unit costs one.
    PagesAxis       => Pages,       cost = 1;
    PinnedBytesAxis => PinnedBytes, cost = 1;
    KernelMetaAxis  => KernelMeta,  cost = 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy-paste slip in the macro above compiles and charges the wrong
    /// row forever.
    #[test]
    fn each_axis_names_its_own_kind() {
        fn check<A: ResourceAxis>(want: ResourceKind) {
            assert_eq!(A::KIND, want);
            assert_eq!(A::NAME, want.name());
            assert_eq!(A::UNIT, want.unit());
            assert_eq!(A::REFUND, want.refund());
        }
        check::<FdSlot>(ResourceKind::FdSlot);
        check::<ObjectRow>(ResourceKind::ObjectRow);
        check::<TaskCount>(ResourceKind::Task);
        check::<ProcCount>(ResourceKind::Process);
        check::<PagesAxis>(ResourceKind::Pages);
        check::<PinnedBytesAxis>(ResourceKind::PinnedBytes);
        check::<CustodyAxis>(ResourceKind::Custody);
        check::<KernelMetaAxis>(ResourceKind::KernelMeta);
    }
}
