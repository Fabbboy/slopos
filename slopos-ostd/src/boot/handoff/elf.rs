//! ELF image payload handoff: a borrowed view over a loader-published
//! image buffer, replacing open-coded `slice::from_raw_parts` at the
//! loader call sites.

use core::ptr::NonNull;

pub struct ElfImage<'a> {
    bytes: &'a [u8],
}

impl<'a> ElfImage<'a> {
    #[inline]
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// First four bytes, or `None` if the payload is shorter than four.
    #[inline]
    pub fn magic(&self) -> Option<[u8; 4]> {
        if self.bytes.len() < 4 {
            return None;
        }
        Some([self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3]])
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Borrow a loader-published ELF image payload as an [`ElfImage`].
/// Returns `None` if the payload is empty or fails the ELF magic-byte
/// probe.
///
/// # Why this is safe to call
///
/// The payload buffer must outlive the kernel, and `len` must bound it;
/// structured reads inside the ELF parser re-bounds-check each field.
pub fn elf_image_handoff(payload: NonNull<u8>, len: usize) -> Option<ElfImage<'static>> {
    if len < 4 {
        return None;
    }
    // SAFETY: caller-published buffer; lifetime exceeds the kernel's
    // load path; `len` bounds the slice.
    let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(payload.as_ptr(), len) };
    if &bytes[0..4] != b"\x7fELF" {
        return None;
    }
    Some(ElfImage { bytes })
}
