//! Coverage as a compile error.
//!
//! A source scanner cannot answer "does type `X` have a field of type
//! `Charge`" — every scanner in this tree is a line-local match with a fixed
//! lookback — and an expansion scan would fail *open* if the type were
//! renamed. The type system can answer it, so it does.
//!
//! [`FileBacking`] requires [`Charged`], and [`Charged`] is sealed to what
//! `#[derive(Charged)]` emits. Every `impl FileBacking for X {}` therefore
//! fails to compile unless `X` carries an object charge, the coercion sites
//! building `KArc<dyn FileBacking>` fail with it, and a backing written years
//! from now is covered the moment it exists.
//!
//! This is why the trait had to move out of `abi`: `abi` may name no `Charge`,
//! because the token is the mechanism and the mechanism lives here where
//! `check_safe_contract_surface.sh` can see it.

use slopos_abi::quota::ObjectRow;

use super::token::Charge;

/// Sealed by construction: the only implementor is what
/// `#[derive(Charged)]` emits.
///
/// A hand-written `impl Charged` in a service crate would have to name this
/// module's private marker, which it cannot.
pub mod sealed {
    /// Implemented only by `#[derive(Charged)]`.
    pub trait ChargedSealed {}
}

/// A type whose existence is accounted for by an object charge.
pub trait Charged: sealed::ChargedSealed {
    /// The charge this object holds, or `None` when the object it is an
    /// alias of is charged elsewhere.
    ///
    /// `None` is not an escape hatch — it is the answer for the two shapes
    /// where a per-value charge would be *wrong* rather than merely absent,
    /// and `#[derive(Charged)]` only emits it for a type that names the
    /// [`AliasOf`] marker:
    ///
    /// - **A pipe**, which has two backings releasing into one registry row.
    ///   A charge in each refunds twice; a charge in one refunds while the
    ///   object is still alive behind the other. The row carries it.
    /// - **A PTY slave open**, where every slave fd aliases one
    ///   `TtySlaveOpen`, so the charge is not per-fd. The master carries it.
    ///
    /// A borrow, never a by-value hand-back: returning the token would
    /// separate it from the object it accounts for.
    fn object_charge(&self) -> Option<&Charge<ObjectRow>>;
}

/// Field marker for a backing whose object is charged somewhere else.
///
/// Zero-sized and constructible only by naming the reason, so "this alias is
/// accounted for elsewhere" is a written claim at the definition site rather
/// than an absence a reader has to notice.
pub struct AliasOf {
    /// Where the charge actually lives. Read by the audit's report.
    pub owner: &'static str,
}

/// One type whose values play both roles: the holder of a shared charge, and
/// an alias of it.
///
/// For a type like a PTY backing, where master and slave are the *same* Rust
/// type distinguished by a field, the charge cannot be a plain `Charge` (the
/// slave has none) and must not be an `Option<Charge>` (`Option::take` is a
/// safe separation, which is what the linearity rule forbids). This enum is
/// the distinct-uncharged-type the rule asks for: there is no `take`, no
/// `Default`, and the alias variant carries a written reason rather than a
/// hole.
pub enum SharedCharge {
    /// This value holds the charge for itself and its aliases.
    Owner(Charge<ObjectRow>),
    /// This value is an alias of an object charged elsewhere.
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
/// teardown. Subsystems implement this on the object that owns their per-open
/// state and put the release logic in its `Drop` — there is no release
/// callback, so a double teardown is unrepresentable and a forgotten one is
/// impossible.
///
/// The [`Charged`] supertrait extends that from lifetime to accounting: the
/// same `Drop` that releases the registry row refunds the charge that made
/// room for it.
pub trait FileBacking: Send + Sync + Charged {}

/// One entry in `.charge_audit_registry`: a charge-bearing type, and how to
/// ask a value of it what it is holding.
///
/// `#[repr(C)]` and a fixed size, because the linker concatenates these into
/// an array that `registry_slice` divides by this stride —
/// `scripts/check_registry_sections.sh` holds the section span to a whole
/// number of them.
#[repr(C)]
pub struct ChargeAuditEntry {
    /// The type carrying the charge, for the audit's report.
    pub type_name: &'static str,
    /// Where it is defined, so a mismatch names a file rather than a type.
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
/// linked into userland binaries whose linker script brackets no such section.
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
