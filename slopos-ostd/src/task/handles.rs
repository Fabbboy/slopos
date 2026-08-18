//! Safe `LinkProvider` hook: one blanket
//! `unsafe impl<T: LinkProvider<R>, R> Linked<R> for T` here lets the kernel
//! write safe impls while the `unsafe` keyword stays interior to OSTD.

use crate::sync::intrusive::{Link, Linked};
use crate::sync::intrusive_dlist::{DLink, DLinked};

/// Kernel-implemented safe trait pointing at a task's `Link<Self, Role>` field.
///
/// [`Linked`] is an `unsafe trait` because consumers rely on stable in-struct
/// field addresses and a distinct field per role — properties of where the
/// trait is impl'd, not of the impl body.
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
