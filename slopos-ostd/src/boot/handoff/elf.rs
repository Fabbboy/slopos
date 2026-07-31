//! ELF image payload handoff.
//!
//! Consolidates the `slice::from_raw_parts(payload, payload_len)` call
//! pattern used by `mm/src/process_vm.rs:781` (ELF relocation pass)
//! and similar loader-published byte buffers.

use core::ptr::NonNull;

/// Lifetime-borrowed view over an ELF image payload.
pub struct ElfImage<'a> {
    bytes: &'a [u8],
}

impl<'a> ElfImage<'a> {
    /// Full ELF image bytes.
    #[inline]
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// First four bytes as an ELF magic-byte tag (`b"\x7fELF"` if the
    /// payload is a valid ELF image).
    #[inline]
    pub fn magic(&self) -> Option<[u8; 4]> {
        if self.bytes.len() < 4 {
            return None;
        }
        Some([self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3]])
    }

    /// Total payload length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// `true` if the payload is empty.
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
/// The payload buffer must outlive the kernel — today's callers
/// (`exec` / `process_vm_load_elf_data`) thread through bootloader-
/// or file-loader-published buffers whose lifetime exceeds the load.
/// Length is supplied by the caller; structured reads inside the ELF
/// parser re-bounds-check each field.
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
