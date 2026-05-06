//! Modern memory-management / address-space surface.
//!
//! Every CR3 write in the kernel funnels through
//! [`cr3::write_cr3_value`]. The module owns:
//!
//!   - typed CR3 primitives (`Cr3Value`, `Pcid`, `MmContextId`)
//!   - per-CPU ASID pools (Linux-style 16-slot dance) — [`asid`]
//!   - TLB shootdown backend (`trait ShootdownBackend`) — [`rar`]
//!   - Lazy Unmap Flush (LUF) ring + cross-CPU drain — [`luf`]
//!   - typed kernel mappings (`KernelMapping`) — [`mapping`]
//!   - CPU errata gating (PCID blacklist) — [`errata`]
//!   - KPTI dual-PML4 scaffolding — [`kpti`]

pub mod asid;
pub mod cr3;
pub mod errata;
pub mod kpti;
pub mod luf;
pub mod luf_hook;
pub mod mapping;
pub mod rar;
pub mod shootdown;

pub use mapping::{KernelMapping, unmap_kernel_page_free};
pub use rar::{IntelRar, ShootdownBackend, SoftwareIpi, backend as shootdown_backend};

pub use asid::{init_ap, init_bsp};
pub use cr3::{MmContextId, Pcid, alloc_mm_context_id, read_cr3_value};
