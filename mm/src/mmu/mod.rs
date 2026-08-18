//! Address-space surface: typed CR3 primitives, per-CPU ASID pools, the TLB
//! shootdown backend, lazy unmap flush, PCID errata gating and KPTI.
//!
//! Every CR3 write in the kernel funnels through [`cr3::write_cr3_value`].

pub mod asid;
pub mod cr3;
pub mod errata;
pub mod kpti;
pub mod luf;
pub mod luf_hook;
pub mod quiesce;
pub mod rar;
pub mod shootdown;

pub use rar::{IntelRar, ShootdownBackend, SoftwareIpi, backend as shootdown_backend};

pub use asid::{init_ap, init_bsp};
pub use cr3::{MmContextId, Pcid, alloc_mm_context_id, read_cr3_value};
