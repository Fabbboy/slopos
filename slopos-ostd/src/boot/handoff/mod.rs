//! Bootloader-published memory boundary primitives.
//!
//! Every `core::slice::from_raw_parts` against bootloader-handed memory goes
//! through one of these; consumers receive `&'static` references or typed views.

pub mod acpi;
pub mod elf;
pub mod framebuffer;
pub mod memmap;

pub use acpi::{acpi_handoff, acpi_region_bytes};
pub use elf::{ElfImage, elf_image_handoff};
pub use framebuffer::{
    Framebuffer, fb_blit_bytes, fb_fill_u8_bulk, fb_ptr_add, fb_write_u8, fb_write_u8_at,
    fb_write_u16, fb_write_u32, fb_write_u32_at, fb_write_u32_unaligned, fb_write_u64_at,
    framebuffer_handoff,
};
pub use memmap::{MemmapEntry, memmap_handoff};
