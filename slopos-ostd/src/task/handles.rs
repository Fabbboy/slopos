//! Safe `LinkProvider` hook that absorbs the kernel-side
//! `unsafe impl Linked<R> for Task` markers via a single blanket
//! `unsafe impl<T: LinkProvider<R>, R> Linked<R> for T` here in OSTD —
//! the kernel impls the safe `LinkProvider` trait and the unsafe
//! `Linked` keyword stays interior to the trusted core.

use crate::sync::intrusive::{Link, Linked};
use crate::sync::intrusive_dlist::{DLink, DLinked};

/// Kernel-implemented safe trait pointing at a task's `Link<Self, Role>`
/// field. A blanket `unsafe impl<T: LinkProvider<R>, R> Linked<R> for T`
/// below absorbs the unsafe contract into OSTD, so the kernel only
/// writes safe `impl LinkProvider for Task` blocks.
///
/// # Why a separate trait
///
/// The underlying [`Linked`] trait is `unsafe trait` because consumers
/// rely on stable in-struct field addresses and distinct fields per
/// role. Those invariants are properties of where the trait is impl'd,
/// not what the impl body says — exactly the kind of guarantee Rust
/// `unsafe trait`s exist to declare. The blanket impl below moves the
/// `unsafe trait` site interior to OSTD; the kernel's per-role impls
/// of `LinkProvider` are safe code.
pub trait LinkProvider<Role>: Sized {
    fn link(&self) -> &Link<Self, Role>;
}

// SAFETY: the `LinkProvider` impl is provided by the inner-type owner
// (kernel-side `Task`), who is responsible for the same stable-address
// and distinct-field-per-role properties the `Linked` trait demands.
// Trust is delegated one level: the unsafe contract is satisfied by
// `LinkProvider` being a kernel-defined impl on a stable kernel type.
unsafe impl<T, R> Linked<R> for T
where
    T: LinkProvider<R>,
{
    #[inline]
    fn link(&self) -> &Link<Self, R> {
        LinkProvider::link(self)
    }
}

/// [`LinkProvider`]'s counterpart for the doubly-linked ownership lists.
/// Same delegation: the kernel writes a safe impl naming the field, and the
/// `unsafe trait` site stays interior to OSTD.
pub trait DLinkProvider<Role>: Sized {
    fn dlink(&self) -> &DLink<Self, Role>;
}

// SAFETY: as with `LinkProvider` above, the stable-address and
// distinct-field-per-role properties are discharged by the kernel-side impl on
// a stable kernel type.
unsafe impl<T, R> DLinked<R> for T
where
    T: DLinkProvider<R>,
{
    #[inline]
    fn dlink(&self) -> &DLink<Self, R> {
        DLinkProvider::dlink(self)
    }
}
