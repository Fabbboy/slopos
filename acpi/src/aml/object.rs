//! Minimal AML value model for the focused I²C-HID enumeration interpreter:
//! enough to run a touchpad's `_INI` and read back the resource template it
//! patches.

use slopos_ostd::KVec;

pub enum AmlVal {
    /// AML integers are 64-bit on revision >= 2.
    Int(u64),
    /// ASCII, no trailing NUL.
    Str(KVec<u8>),
    Buf(KVec<u8>),
    Package(KVec<AmlVal>),
    Uninit,
}

impl AmlVal {
    /// Non-integers yield 0; buffers and strings are deliberately not coerced,
    /// as the enumeration path never relies on it.
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

/// Best-effort: an allocation failure truncates the copy, which callers
/// surface as a parse failure downstream.
pub fn clone_bytes(src: &KVec<u8>) -> KVec<u8> {
    let mut out = KVec::new();
    for &b in src.iter() {
        if out.push(b).is_err() {
            break;
        }
    }
    out
}

/// Best-effort: an allocation failure truncates the copy.
pub fn bytes_from_slice(src: &[u8]) -> KVec<u8> {
    let mut out = KVec::new();
    for &b in src {
        if out.push(b).is_err() {
            break;
        }
    }
    out
}

/// Little-endian, so the key preserves the NameSeg's in-memory byte order.
#[inline]
pub fn nameseg_key(seg: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*seg)
}
