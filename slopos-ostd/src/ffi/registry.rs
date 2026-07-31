//! Linker-built registries.
//!
//! A registry is a linker section that a macro drops `#[used]` statics into
//! and that the kernel later walks as a contiguous array. `link.ld` brackets
//! each one with `__start_<section>` / `__stop_<section>` symbols.
//!
//! OSTD owns the whole mechanism: the section names, the bracket symbol
//! declarations, and the walk. A consumer crate names a registry by
//! [`RegistryId`] and never spells a section string, so it cannot place a
//! static into an arbitrary section — including one the linker script's
//! `*(.text .text.*)`-style wildcards would silently merge into an existing
//! output section, where no post-link check could see it.
//!
//! Writer and reader are tied together by [`RegistryEntry`]: the entry type
//! declares which registries it belongs to, [`registry_entry!`] refuses to
//! emit a static whose type does not claim the registry it is being placed
//! in, and [`registry_slice`] reads back through the same declaration.
//!
//! [`registry_entry!`]: crate::registry_entry

use core::mem::size_of;

/// The registries `link.ld` defines.
///
/// `#[repr(u8)]` so the [`registry_entry!`](crate::registry_entry)
/// consistency check can compare discriminants in a `const` context.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RegistryId {
    /// `.boot_init_early_hw`
    BootInitEarlyHw,
    /// `.boot_init_memory`
    BootInitMemory,
    /// `.boot_init_drivers`
    BootInitDrivers,
    /// `.boot_init_services`
    BootInitServices,
    /// `.boot_init_optional`
    BootInitOptional,
    /// `.driver_registry`
    PciDrivers,
    /// `.platform_driver_registry`
    PlatformDrivers,
    /// `.test_registry`
    Tests,
    /// `.hermetic_state_registry`
    HermeticStates,
}

impl RegistryId {
    /// The section label, for diagnostics. The label the macro actually
    /// emits is a literal in [`registry_entry!`](crate::registry_entry)'s
    /// body, because an attribute needs one.
    pub const fn section(self) -> &'static str {
        match self {
            RegistryId::BootInitEarlyHw => ".boot_init_early_hw",
            RegistryId::BootInitMemory => ".boot_init_memory",
            RegistryId::BootInitDrivers => ".boot_init_drivers",
            RegistryId::BootInitServices => ".boot_init_services",
            RegistryId::BootInitOptional => ".boot_init_optional",
            RegistryId::PciDrivers => ".driver_registry",
            RegistryId::PlatformDrivers => ".platform_driver_registry",
            RegistryId::Tests => ".test_registry",
            RegistryId::HermeticStates => ".hermetic_state_registry",
        }
    }
}

/// A type that may be emitted into a linker registry.
///
/// Implemented by the crate that owns the entry type — OSTD cannot name
/// `PciDriverEntry` or `TestDesc`. Declaring the membership here is what
/// lets the writer macro and [`registry_slice`] agree on the element type
/// without either side restating it.
///
/// `BootInitStep` lists five registries because the boot phases are five
/// separate sections holding one entry type.
pub trait RegistryEntry: Sized + 'static {
    /// Registries this type may be placed in.
    const REGISTRIES: &'static [RegistryId];
}

/// `const`-callable membership test, used by
/// [`registry_entry!`](crate::registry_entry)'s consistency assertion.
#[doc(hidden)]
pub const fn __declares(registries: &[RegistryId], id: RegistryId) -> bool {
    let mut i = 0;
    while i < registries.len() {
        if registries[i] as u8 == id as u8 {
            return true;
        }
        i += 1;
    }
    false
}

// The bracket symbols the linker synthesises around each section. Declared
// as `u8` rather than the entry type: the symbols are addresses, not values,
// and giving them a type here would be a second, unchecked claim about what
// the section holds.
unsafe extern "C" {
    static __start_boot_init_early_hw: u8;
    static __stop_boot_init_early_hw: u8;
    static __start_boot_init_memory: u8;
    static __stop_boot_init_memory: u8;
    static __start_boot_init_drivers: u8;
    static __stop_boot_init_drivers: u8;
    static __start_boot_init_services: u8;
    static __stop_boot_init_services: u8;
    static __start_boot_init_optional: u8;
    static __stop_boot_init_optional: u8;
    static __start_driver_registry: u8;
    static __stop_driver_registry: u8;
    static __start_platform_driver_registry: u8;
    static __stop_platform_driver_registry: u8;
    static __start_test_registry: u8;
    static __stop_test_registry: u8;
    static __start_hermetic_state_registry: u8;
    static __stop_hermetic_state_registry: u8;
}

fn bounds(id: RegistryId) -> (*const u8, *const u8) {
    match id {
        RegistryId::BootInitEarlyHw => (
            &raw const __start_boot_init_early_hw,
            &raw const __stop_boot_init_early_hw,
        ),
        RegistryId::BootInitMemory => (
            &raw const __start_boot_init_memory,
            &raw const __stop_boot_init_memory,
        ),
        RegistryId::BootInitDrivers => (
            &raw const __start_boot_init_drivers,
            &raw const __stop_boot_init_drivers,
        ),
        RegistryId::BootInitServices => (
            &raw const __start_boot_init_services,
            &raw const __stop_boot_init_services,
        ),
        RegistryId::BootInitOptional => (
            &raw const __start_boot_init_optional,
            &raw const __stop_boot_init_optional,
        ),
        RegistryId::PciDrivers => (
            &raw const __start_driver_registry,
            &raw const __stop_driver_registry,
        ),
        RegistryId::PlatformDrivers => (
            &raw const __start_platform_driver_registry,
            &raw const __stop_platform_driver_registry,
        ),
        RegistryId::Tests => (
            &raw const __start_test_registry,
            &raw const __stop_test_registry,
        ),
        RegistryId::HermeticStates => (
            &raw const __start_hermetic_state_registry,
            &raw const __stop_hermetic_state_registry,
        ),
    }
}

/// Borrow the contiguous `[T]` the linker built for `id`.
///
/// Panics if `T` does not declare `id`, or if the section's byte span is not
/// a whole number of `T`s — the latter is what a wrong-typed entry looks
/// like from here, and computing a count from it would be unsound rather
/// than merely wrong.
pub fn registry_slice<T: RegistryEntry>(id: RegistryId) -> &'static [T] {
    assert!(
        __declares(T::REGISTRIES, id),
        "registry_slice: entry type does not declare this registry",
    );
    let (start, stop) = bounds(id);
    let bytes = (stop as usize).saturating_sub(start as usize);
    let stride = size_of::<T>();
    assert!(stride != 0, "registry_slice: zero-sized entry type");
    assert!(
        bytes % stride == 0,
        "registry_slice: section span is not a whole number of entries",
    );
    // SAFETY: `link.ld` brackets the section with these two symbols and the
    // only way to place a static between them is `registry_entry!`, which
    // refuses any type that does not declare this registry. The span is a
    // whole number of `T`s per the assertion above, and the section is part
    // of the image, so the entries live for `'static` and are never written
    // after load.
    unsafe { core::slice::from_raw_parts(start.cast::<T>(), bytes / stride) }
}

/// Emit a `#[used]` static into a linker registry.
///
/// The section label lives in this macro's body, so a consumer crate picks
/// from the closed set `link.ld` defines rather than supplying a string.
/// The expansion also asserts that the static's type declares the registry
/// it is being placed into, which is what stops a reader walking the section
/// as the wrong type.
///
/// ```ignore
/// slopos_ostd::registry_entry! {
///     pci_drivers,
///     pub static VIRTIO_NET_DRIVER: PciDriverEntry = PciDriverEntry { … };
/// }
/// ```
#[macro_export]
#[allow_internal_unsafe]
macro_rules! registry_entry {
    (boot_init_early_hw, $($item:tt)*) => {
        $crate::__registry_entry!(
            ".boot_init_early_hw", BootInitEarlyHw, $($item)*);
    };
    (boot_init_memory, $($item:tt)*) => {
        $crate::__registry_entry!(".boot_init_memory", BootInitMemory, $($item)*);
    };
    (boot_init_drivers, $($item:tt)*) => {
        $crate::__registry_entry!(".boot_init_drivers", BootInitDrivers, $($item)*);
    };
    (boot_init_services, $($item:tt)*) => {
        $crate::__registry_entry!(".boot_init_services", BootInitServices, $($item)*);
    };
    (boot_init_optional, $($item:tt)*) => {
        $crate::__registry_entry!(".boot_init_optional", BootInitOptional, $($item)*);
    };
    (pci_drivers, $($item:tt)*) => {
        $crate::__registry_entry!(".driver_registry", PciDrivers, $($item)*);
    };
    (platform_drivers, $($item:tt)*) => {
        $crate::__registry_entry!(".platform_driver_registry", PlatformDrivers, $($item)*);
    };
    (tests, $($item:tt)*) => {
        $crate::__registry_entry!(".test_registry", Tests, $($item)*);
    };
    (hermetic_states, $($item:tt)*) => {
        $crate::__registry_entry!(".hermetic_state_registry", HermeticStates, $($item)*);
    };
}

#[doc(hidden)]
#[macro_export]
#[allow_internal_unsafe]
macro_rules! __registry_entry {
    (
        $section:literal, $id:ident,
        $(#[$attr:meta])*
        $vis:vis static $name:ident : $ty:ty = $init:expr ;
    ) => {
        const _: () = assert!(
            $crate::ffi::registry::__declares(
                <$ty as $crate::ffi::registry::RegistryEntry>::REGISTRIES,
                $crate::ffi::registry::RegistryId::$id,
            ),
            "static's type does not declare this registry",
        );
        $(#[$attr])*
        #[used]
        #[unsafe(link_section = $section)]
        $vis static $name : $ty = $init;
    };
}
