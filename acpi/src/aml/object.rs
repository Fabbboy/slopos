//! Minimal AML value model used by the focused I²C-HID enumeration
//! interpreter. We only need integers, strings, and (mutable) buffers —
//! enough to run a touchpad device's `_INI` and read back the resource
//! template it patches.

use slopos_ostd::KVec;

/// An AML runtime value.
pub enum AmlVal {
    /// Integer (AML integers are 64-bit on revision ≥ 2).
    Int(u64),
    /// ASCII string (no trailing NUL).
    Str(KVec<u8>),
    /// Byte buffer (e.g. a `ResourceTemplate`).
    Buf(KVec<u8>),
    /// Package (ordered list of values, e.g. a GPIO pad-info table).
    Package(KVec<AmlVal>),
    /// Declared but unset.
    Uninit,
}

impl AmlVal {
    /// Coerce to an integer where the AML rules allow it (`Uninit`/other
    /// → 0). Buffers/strings are not numerically coerced here (the
    /// touchpad methods never do that on the enumeration path).
    pub fn as_int(&self) -> u64 {
        match self {
            AmlVal::Int(v) => *v,
            _ => 0,
        }
    }

    pub fn clone_val(&self) -> AmlVal {
        match self {
            AmlVal::Int(v) => AmlVal::Int(*v),
            AmlVal::Str(s) => AmlVal::Str(clone_bytes(s)),
            AmlVal::Buf(b) => AmlVal::Buf(clone_bytes(b)),
            AmlVal::Package(elems) => {
                let mut out = KVec::new();
                for e in elems.iter() {
                    if out.push(e.clone_val()).is_err() {
                        break;
                    }
                }
                AmlVal::Package(out)
            }
            AmlVal::Uninit => AmlVal::Uninit,
        }
    }
}

/// Clone a byte vector (best-effort; an allocation failure yields an
/// empty vector — the caller treats that as a parse failure downstream).
pub fn clone_bytes(src: &KVec<u8>) -> KVec<u8> {
    let mut out = KVec::new();
    for &b in src.iter() {
        if out.push(b).is_err() {
            break;
        }
    }
    out
}

/// Build a [`KVec<u8>`] from a slice (best-effort).
pub fn bytes_from_slice(src: &[u8]) -> KVec<u8> {
    let mut out = KVec::new();
    for &b in src {
        if out.push(b).is_err() {
            break;
        }
    }
    out
}

/// Pack a 4-byte ACPI NameSeg into a `u32` key (little-endian, the
/// natural in-memory order). Used as a map key for method arg-counts and
/// device/field lookups.
#[inline]
pub fn nameseg_key(seg: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*seg)
}
