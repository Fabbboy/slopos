//! Read-back of the unwinder's `.eh_frame_hdr` FDE index.
//!
//! The index is what makes a return-address lookup a binary search instead
//! of a full parse of every FDE in `.eh_frame`. Nothing at runtime notices
//! when it is missing — `EhFrameHdr::parse`'s error is swallowed and the
//! finder falls through to a linear scan — so the in-kernel tests read the
//! header directly and assert its shape.
//!
//! The raw reads live here because `slopos-ostd` is the only kernel crate
//! that may hold `unsafe`.

/// The `.eh_frame_hdr` prologue: version, three encoding bytes, the
/// `.eh_frame` pointer, and the entry count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnwindIndexHeader {
    pub version: u8,
    pub eh_frame_ptr_enc: u8,
    pub fde_count_enc: u8,
    pub table_enc: u8,
    /// Resolved absolute address of `.eh_frame`.
    pub eh_frame_ptr: u64,
    pub fde_count: u32,
}

/// `DW_EH_PE_pcrel | DW_EH_PE_sdata4`, what lld emits for the `.eh_frame`
/// pointer.
pub const ENC_PCREL_SDATA4: u8 = 0x1b;
/// `DW_EH_PE_udata4`, the entry count.
pub const ENC_UDATA4: u8 = 0x03;
/// `DW_EH_PE_datarel | DW_EH_PE_sdata4`, the search-table entries — both
/// halves relative to the start of `.eh_frame_hdr`.
pub const ENC_DATAREL_SDATA4: u8 = 0x3b;

#[cfg(target_os = "none")]
mod imp {
    use super::{ENC_DATAREL_SDATA4, UnwindIndexHeader};

    unsafe extern "C" {
        static __GNU_EH_FRAME_HDR: u8;
        static __eh_frame: u8;
    }

    fn hdr_base() -> *const u8 {
        core::ptr::addr_of!(__GNU_EH_FRAME_HDR)
    }

    /// Address `link.ld` gave `.eh_frame`.
    pub fn eh_frame_addr() -> u64 {
        core::ptr::addr_of!(__eh_frame) as u64
    }

    /// Decode the 12-byte prologue.
    pub fn header() -> UnwindIndexHeader {
        let base = hdr_base();
        // SAFETY: `link.ld` brackets `.eh_frame_hdr` with this symbol and the
        // gate in `scripts/check_registry_sections.sh` fails the build unless
        // the section holds at least a prologue plus one entry, so the first
        // 12 bytes are inside it.
        let raw = unsafe { core::slice::from_raw_parts(base, 12) };
        let ptr_rel = i32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
        UnwindIndexHeader {
            version: raw[0],
            eh_frame_ptr_enc: raw[1],
            fde_count_enc: raw[2],
            table_enc: raw[3],
            // pcrel against the field's own address, which is base + 4.
            eh_frame_ptr: (base as u64)
                .wrapping_add(4)
                .wrapping_add(ptr_rel as i64 as u64),
            fde_count: u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
        }
    }

    /// Start address of the highest-addressed function the index covers.
    ///
    /// The search table is sorted by initial location, so the last entry is
    /// the one whose FDE no linear scan would reach early — the worst case
    /// for the unwinder and therefore the probe the timing test uses.
    pub fn highest_indexed_function() -> Option<u64> {
        let hdr = header();
        if hdr.version != 1 || hdr.table_enc != ENC_DATAREL_SDATA4 || hdr.fde_count == 0 {
            return None;
        }
        let base = hdr_base();
        let offset = 12usize.checked_add((hdr.fde_count as usize - 1).checked_mul(8)?)?;
        // SAFETY: `fde_count` entries of 8 bytes follow the prologue; the
        // section span is held to exactly that shape by the build gate.
        let raw = unsafe { core::slice::from_raw_parts(base.add(offset), 4) };
        let rel = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        Some((base as u64).wrapping_add(rel as i64 as u64))
    }

    /// Resolve `pc` to the start of its enclosing function through the
    /// unwinder's own finder, or `None` when no FDE covers it.
    pub fn enclosing_function(pc: u64) -> Option<u64> {
        let found = unwinding::abi::_Unwind_FindEnclosingFunction(pc as *mut core::ffi::c_void);
        if found.is_null() {
            None
        } else {
            Some(found as u64)
        }
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    use super::UnwindIndexHeader;

    pub fn eh_frame_addr() -> u64 {
        0
    }

    pub fn header() -> UnwindIndexHeader {
        UnwindIndexHeader {
            version: 0,
            eh_frame_ptr_enc: 0,
            fde_count_enc: 0,
            table_enc: 0,
            eh_frame_ptr: 0,
            fde_count: 0,
        }
    }

    pub fn highest_indexed_function() -> Option<u64> {
        None
    }

    pub fn enclosing_function(_pc: u64) -> Option<u64> {
        None
    }
}

pub use imp::{eh_frame_addr, enclosing_function, header, highest_indexed_function};
