//! Bootloader-published memory boundary primitives.
//!
//! Every `core::slice::from_raw_parts` call against memory the
//! bootloader hands the kernel goes through one of these primitives.
//! The unsafe block lives interior to OSTD; consumers receive
//! `&'static` references or typed views.
//!
//! Today's call sites in the kernel (`acpi/src/tables.rs`,
//! `mm/src/process_vm.rs`, `utils/src/boot_info.rs`,
//! `video/src/framebuffer.rs`) keep their existing shape — this
//! module just exposes the safe primitives so future consumer
//! migrations can collapse the boundary unsafe interior to OSTD.

pub mod acpi;
pub mod elf;
pub mod framebuffer;
pub mod memmap;

pub use acpi::{acpi_handoff, acpi_region_bytes};
pub use elf::{ElfImage, elf_image_handoff};
pub use framebuffer::{
    Framebuffer, fb_checked_ptr, fb_copy_bytes, fb_copy_bytes_raw, fb_write_3bytes,
    fb_write_u16_unaligned, fb_write_u32_unaligned, framebuffer_handoff,
};
pub use memmap::{MemmapEntry, memmap_handoff};
