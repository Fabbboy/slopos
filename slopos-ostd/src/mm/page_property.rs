//! Typed page-protection properties.
//!
//! `PageProperty` is the safe-Rust description a [`CursorMut`] consumer
//! passes to `map` / `protect`. Conversion to/from the on-disk PTE bit
//! pattern lives in [`crate::mm::page_table::PteFlags`]; this module
//! only carries the typed fields and the [`CachePolicy`] enum.
//!
//! [`CursorMut`]: crate::mm::vm_space::CursorMut

use crate::mm::page_table::PteFlags;

/// Cache attribute for a leaf mapping.
///
/// Maps onto the PWT/PCD/PAT bit triple in the PTE. The PAT MSR layout
/// SlopOS uses (`mm/src/pat.rs` today) is the firmware default, so:
///
/// | variant            | PWT | PCD | PAT |
/// |--------------------|-----|-----|-----|
/// | `WriteBack`        | 0   | 0   | 0   |
/// | `WriteCombining`   | 1   | 0   | 0   |
/// | `Uncacheable`      | 0   | 1   | 0   |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachePolicy {
    WriteBack,
    WriteCombining,
    Uncacheable,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self::WriteBack
    }
}

/// Typed access/cache properties of a single leaf mapping.
///
/// `read` is always `true` on x86_64 — a present PTE is implicitly
/// readable — but the field is carried for forward-compat with ARM64,
/// which has a separate read bit.
///
/// `software` carries the three "available to OS" PTE bits (9..=11)
/// through cursor `query` / `map` / `protect` round-trips so consumers
/// can encode kernel-policy state (e.g. copy-on-write markers) in the
/// page table itself rather than in parallel bookkeeping. Only the
/// low 3 bits are valid; higher bits are masked off in `to_leaf_flags`.
/// OSTD assigns no semantics to the field — it is opaque storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageProperty {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
    pub user: bool,
    pub cache_policy: CachePolicy,
    pub global: bool,
    pub software: u8,
}

impl PageProperty {
    /// `software` bit meaning "this leaf owns no `MetaSlot` reference".
    /// Set by [`CursorMut::map_io`] over physical memory that has no
    /// slot at all — device apertures, firmware runtime regions — and
    /// read back by [`CursorMut::unmap`], which then clears the entry
    /// without attempting to reclaim a reference that was never taken.
    ///
    /// PTE bit 10. Bit 9 (`software & 1`) is the consumer's
    /// copy-on-write marker and must stay free.
    ///
    /// [`CursorMut::map_io`]: crate::mm::vm_space::CursorMut::map_io
    /// [`CursorMut::unmap`]: crate::mm::vm_space::CursorMut::unmap
    pub const SOFTWARE_NO_FRAME_REF: u8 = 0b010;

    pub const KERNEL_RW: Self = Self {
        read: true,
        write: true,
        execute: false,
        user: false,
        cache_policy: CachePolicy::WriteBack,
        global: true,
        software: 0,
    };

    pub const KERNEL_RO: Self = Self {
        read: true,
        write: false,
        execute: false,
        user: false,
        cache_policy: CachePolicy::WriteBack,
        global: true,
        software: 0,
    };

    pub const USER_RW: Self = Self {
        read: true,
        write: true,
        execute: false,
        user: true,
        cache_policy: CachePolicy::WriteBack,
        global: false,
        software: 0,
    };

    pub const USER_RO: Self = Self {
        read: true,
        write: false,
        execute: false,
        user: true,
        cache_policy: CachePolicy::WriteBack,
        global: false,
        software: 0,
    };

    pub const USER_RX: Self = Self {
        read: true,
        write: false,
        execute: true,
        user: true,
        cache_policy: CachePolicy::WriteBack,
        global: false,
        software: 0,
    };

    /// Encode as a leaf [`PteFlags`] bit pattern (does **not** include
    /// the physical address). The HUGE bit is **not** set here — the
    /// caller's `CursorMut::map::<S>` ORs it in based on `S::HUGE_BIT`.
    pub fn to_leaf_flags(self) -> PteFlags {
        let mut f = PteFlags::PRESENT;
        if self.write {
            f |= PteFlags::WRITABLE;
        }
        if self.user {
            f |= PteFlags::USER;
        }
        if !self.execute {
            f |= PteFlags::NO_EXECUTE;
        }
        if self.global {
            f |= PteFlags::GLOBAL;
        }
        match self.cache_policy {
            CachePolicy::WriteBack => {}
            CachePolicy::WriteCombining => f |= PteFlags::WRITE_THROUGH,
            CachePolicy::Uncacheable => f |= PteFlags::CACHE_DISABLE,
        }
        // Mask `software` to its low 3 bits so a stray higher bit
        // cannot collide with the HUGE flag (bit 7) or the address
        // mask (bits 12..=51).
        let sw_bits = ((self.software as u64) & 0x7) << PteFlags::SOFTWARE_BITS_SHIFT;
        PteFlags::from_bits_truncate(f.bits() | sw_bits)
    }

    /// Decode from the leaf PTE bit pattern. Reads the access/cache
    /// bits and the AVL software bits (9..=11); the address and the
    /// HUGE bit are intentionally not surfaced (caller knows the leaf
    /// size from the cursor's level).
    pub fn from_leaf_flags(flags: PteFlags) -> Self {
        let cache_policy = if flags.contains(PteFlags::CACHE_DISABLE) {
            CachePolicy::Uncacheable
        } else if flags.contains(PteFlags::WRITE_THROUGH) {
            CachePolicy::WriteCombining
        } else {
            CachePolicy::WriteBack
        };
        let software =
            ((flags.bits() & PteFlags::SOFTWARE_BITS_MASK) >> PteFlags::SOFTWARE_BITS_SHIFT) as u8;
        Self {
            read: flags.contains(PteFlags::PRESENT),
            write: flags.contains(PteFlags::WRITABLE),
            execute: !flags.contains(PteFlags::NO_EXECUTE),
            user: flags.contains(PteFlags::USER),
            cache_policy,
            global: flags.contains(PteFlags::GLOBAL),
            software,
        }
    }
}

impl Default for PageProperty {
    fn default() -> Self {
        Self::USER_RO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_user_rw() {
        let p = PageProperty::USER_RW;
        let f = p.to_leaf_flags();
        assert!(f.contains(PteFlags::PRESENT));
        assert!(f.contains(PteFlags::WRITABLE));
        assert!(f.contains(PteFlags::USER));
        assert!(f.contains(PteFlags::NO_EXECUTE));
        assert!(!f.contains(PteFlags::GLOBAL));
        assert_eq!(PageProperty::from_leaf_flags(f), p);
    }

    #[test]
    fn round_trip_kernel_rw() {
        let p = PageProperty::KERNEL_RW;
        let f = p.to_leaf_flags();
        assert!(f.contains(PteFlags::WRITABLE));
        assert!(!f.contains(PteFlags::USER));
        assert!(f.contains(PteFlags::GLOBAL));
        assert_eq!(PageProperty::from_leaf_flags(f), p);
    }

    #[test]
    fn round_trip_user_rx() {
        let p = PageProperty::USER_RX;
        let f = p.to_leaf_flags();
        assert!(!f.contains(PteFlags::WRITABLE));
        assert!(f.contains(PteFlags::USER));
        assert!(!f.contains(PteFlags::NO_EXECUTE));
        assert_eq!(PageProperty::from_leaf_flags(f), p);
    }

    #[test]
    fn user_false_clears_user_bit() {
        let p = PageProperty {
            user: false,
            ..PageProperty::USER_RW
        };
        let f = p.to_leaf_flags();
        assert!(!f.contains(PteFlags::USER));
    }

    #[test]
    fn execute_false_sets_nx() {
        let p = PageProperty {
            execute: false,
            ..PageProperty::USER_RX
        };
        assert!(p.to_leaf_flags().contains(PteFlags::NO_EXECUTE));
    }

    #[test]
    fn cache_policy_write_combining_round_trip() {
        let p = PageProperty {
            cache_policy: CachePolicy::WriteCombining,
            ..PageProperty::USER_RW
        };
        let f = p.to_leaf_flags();
        assert!(f.contains(PteFlags::WRITE_THROUGH));
        assert!(!f.contains(PteFlags::CACHE_DISABLE));
        assert_eq!(
            PageProperty::from_leaf_flags(f).cache_policy,
            CachePolicy::WriteCombining
        );
    }

    #[test]
    fn cache_policy_uncacheable_round_trip() {
        let p = PageProperty {
            cache_policy: CachePolicy::Uncacheable,
            ..PageProperty::USER_RW
        };
        let f = p.to_leaf_flags();
        assert!(f.contains(PteFlags::CACHE_DISABLE));
        assert_eq!(
            PageProperty::from_leaf_flags(f).cache_policy,
            CachePolicy::Uncacheable
        );
    }

    #[test]
    fn software_bits_round_trip_each_value() {
        for software in 0u8..=7u8 {
            let p = PageProperty {
                software,
                ..PageProperty::USER_RW
            };
            let f = p.to_leaf_flags();
            assert_eq!(
                f.bits() & PteFlags::SOFTWARE_BITS_MASK,
                (software as u64) << PteFlags::SOFTWARE_BITS_SHIFT,
                "software={software}: PTE bits did not match",
            );
            assert_eq!(PageProperty::from_leaf_flags(f).software, software);
        }
    }

    #[test]
    fn software_bits_above_three_bits_get_masked() {
        let p = PageProperty {
            software: 0xFF,
            ..PageProperty::USER_RW
        };
        let f = p.to_leaf_flags();
        // Only bits 9..=11 of the PTE may be set by the software field.
        assert_eq!(
            f.bits() & PteFlags::SOFTWARE_BITS_MASK,
            PteFlags::SOFTWARE_BITS_MASK,
            "software=0xFF should set all three AVL bits"
        );
        // Adjacent bits must remain untouched.
        assert!(!f.contains(PteFlags::HUGE)); // bit 7
        // No address bits set.
        assert_eq!(f.bits() & PteFlags::ADDRESS_MASK, 0);
        // Round-trip preserves the masked value.
        assert_eq!(PageProperty::from_leaf_flags(f).software, 0x7);
    }

    #[test]
    fn software_bits_independent_of_hardware_flags() {
        let p = PageProperty {
            software: 0x5,
            ..PageProperty::USER_RW
        };
        let f = p.to_leaf_flags();
        assert!(f.contains(PteFlags::PRESENT));
        assert!(f.contains(PteFlags::WRITABLE));
        assert!(f.contains(PteFlags::USER));
        assert!(f.contains(PteFlags::NO_EXECUTE));
        let decoded = PageProperty::from_leaf_flags(f);
        assert_eq!(decoded, p);
    }
}
