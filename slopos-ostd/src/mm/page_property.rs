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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageProperty {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
    pub user: bool,
    pub cache_policy: CachePolicy,
    pub global: bool,
}

impl PageProperty {
    pub const KERNEL_RW: Self = Self {
        read: true,
        write: true,
        execute: false,
        user: false,
        cache_policy: CachePolicy::WriteBack,
        global: true,
    };

    pub const KERNEL_RO: Self = Self {
        read: true,
        write: false,
        execute: false,
        user: false,
        cache_policy: CachePolicy::WriteBack,
        global: true,
    };

    pub const USER_RW: Self = Self {
        read: true,
        write: true,
        execute: false,
        user: true,
        cache_policy: CachePolicy::WriteBack,
        global: false,
    };

    pub const USER_RO: Self = Self {
        read: true,
        write: false,
        execute: false,
        user: true,
        cache_policy: CachePolicy::WriteBack,
        global: false,
    };

    pub const USER_RX: Self = Self {
        read: true,
        write: false,
        execute: true,
        user: true,
        cache_policy: CachePolicy::WriteBack,
        global: false,
    };

    /// Encode as a leaf [`PteFlags`] bit pattern (does **not** include
    /// the physical address).
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
        f
    }

    /// Decode from the leaf PTE bit pattern. Reads only the
    /// access/cache bits; the address and HUGE/COW etc. are ignored.
    pub fn from_leaf_flags(flags: PteFlags) -> Self {
        let cache_policy = if flags.contains(PteFlags::CACHE_DISABLE) {
            CachePolicy::Uncacheable
        } else if flags.contains(PteFlags::WRITE_THROUGH) {
            CachePolicy::WriteCombining
        } else {
            CachePolicy::WriteBack
        };
        Self {
            read: flags.contains(PteFlags::PRESENT),
            write: flags.contains(PteFlags::WRITABLE),
            execute: !flags.contains(PteFlags::NO_EXECUTE),
            user: flags.contains(PteFlags::USER),
            cache_policy,
            global: flags.contains(PteFlags::GLOBAL),
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
}
