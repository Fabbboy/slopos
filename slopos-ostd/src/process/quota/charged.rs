//! Coverage as a compile error.
//!
//! [`FileBacking`] requires [`Charged`], which is sealed to what
//! `#[derive(Charged)]` emits, so `impl FileBacking for X {}` fails to compile
//! unless `X` carries an object charge. A source scanner could not answer that
//! question; the type system can.

use slopos_abi::quota::ObjectRow;

use super::token::Charge;

/// Sealed by construction: the only implementor is what
/// `#[derive(Charged)]` emits.
pub mod sealed {
    pub trait ChargedSealed {}
}

/// A type whose existence is accounted for by an object charge.
pub trait Charged: sealed::ChargedSealed {
    /// The charge this object holds, or `None` when the object it is an
    /// alias of is charged elsewhere.
    ///
    /// `#[derive(Charged)]` emits `None` only for a type naming [`AliasOf`]:
    /// a pipe's two backings release into one registry row, and every PTY
    /// slave fd aliases one `TtySlaveOpen`.
    ///
    /// A borrow, never a by-value hand-back: returning the token would
    /// separate it from the object it accounts for.
    fn object_charge(&self) -> Option<&Charge<ObjectRow>>;
}

/// Field marker for a backing whose object is charged somewhere else.
pub struct AliasOf {
    /// Where the charge actually lives.
    pub owner: &'static str,
}

/// One type whose values play both roles: the holder of a shared charge, and
/// an alias of it.
///
/// Not `Option<Charge>`: `Option::take` is a safe separation, which the
/// linearity rule forbids.
pub enum SharedCharge {
    Owner(Charge<ObjectRow>),
    Alias(AliasOf),
}

impl SharedCharge {
    /// The charge, when this value is the one holding it.
    #[inline]
    pub fn get(&self) -> Option<&Charge<ObjectRow>> {
        match self {
            Self::Owner(charge) => Some(charge),
            Self::Alias(_) => None,
        }
    }
}

/// Owned per-open backing object of a file description.
///
/// The open-file layer holds each open file's backing as a
/// `KArc<dyn FileBacking>`; dropping the last strong reference **is** the
/// teardown, so release logic goes in `Drop` and there is no release callback
/// to run twice or forget.
///
/// The [`Charged`] supertrait extends that from lifetime to accounting: the
/// same `Drop` that releases the registry row refunds the charge.
pub trait FileBacking: Send + Sync + Charged {}

/// One entry in `.charge_audit_registry`: a charge-bearing type, and where it
/// is defined.
///
/// `#[repr(C)]` and a fixed size: the linker concatenates these into an array
/// that `registry_slice` divides by this stride.
#[repr(C)]
pub struct ChargeAuditEntry {
    pub type_name: &'static str,
    pub file: &'static str,
    pub line: u32,
    pub _pad: u32,
}

const _: () = assert!(core::mem::size_of::<ChargeAuditEntry>() == 40);
const _: () = assert!(core::mem::align_of::<ChargeAuditEntry>() == 8);

impl crate::ffi::registry::RegistryEntry for ChargeAuditEntry {
    const REGISTRIES: &'static [crate::ffi::registry::RegistryId] =
        &[crate::ffi::registry::RegistryId::ChargeAudit];
}

/// Every registered charge-bearing type, in link order.
pub fn charge_audit_entries() -> &'static [ChargeAuditEntry] {
    crate::ffi::registry::registry_slice::<ChargeAuditEntry>(
        crate::ffi::registry::RegistryId::ChargeAudit,
    )
}

/// Register a charge-bearing type with the audit.
///
/// Invoked from the crate that *defines* the type, never from OSTD: this
/// places a `#[used]` static in a kernel-only linker section, and OSTD is
/// linked into userland binaries too.
#[macro_export]
macro_rules! charge_audit {
    ($ty:ty) => {
        $crate::__paste::paste! {
            $crate::registry_entry! {
                charge_audit,
                #[allow(non_upper_case_globals)]
                pub static [<CHARGE_AUDIT_ $ty>]:
                    $crate::process::quota::ChargeAuditEntry =
                    $crate::process::quota::ChargeAuditEntry {
                        type_name: ::core::stringify!($ty),
                        file: ::core::file!(),
                        line: ::core::line!(),
                        _pad: 0,
                    };
            }
        }
    };
}
