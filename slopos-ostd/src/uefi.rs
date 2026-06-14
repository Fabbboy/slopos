//! UEFI Runtime Services `ResetSystem` call.
//!
//! On a UEFI-booted platform the firmware exposes a Runtime Services table
//! that stays callable after `ExitBootServices`. Its `ResetSystem` is the
//! most reliable reboot / power-off mechanism on modern hardware: the
//! firmware runs the chipset-correct sequence internally. SlopOS uses it as
//! the first-choice reboot/shutdown method, falling back to the ACPI paths.
//!
//! This module owns only the `unsafe` call; the caller must have mapped the
//! EFI Runtime Services regions into the active address space (see
//! `boot::uefi_runtime`).

use core::ffi::c_void;

/// `EFI_RESET_TYPE` (UEFI 2.x §8.5.1 ResetSystem).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EfiResetType {
    /// Cold reset: processors and devices to their initial state.
    Cold = 0,
    /// Warm reset: system-wide init, pending cycles preserved.
    Warm = 1,
    /// Power off (ACPI G2/S5 or G3).
    Shutdown = 2,
}

/// Byte offset of the `RuntimeServices` pointer within `EFI_SYSTEM_TABLE`
/// (after the 24-byte `EFI_TABLE_HEADER` and the console/vendor fields).
const SYSTEM_TABLE_RUNTIME_SERVICES: usize = 88;
/// Byte offset of `ResetSystem` within `EFI_RUNTIME_SERVICES`.
const RUNTIME_SERVICES_RESET_SYSTEM: usize = 104;
/// `EFI_TABLE_HEADER.Signature` for `EFI_RUNTIME_SERVICES` ("RUNTSERV").
const RUNTIME_SERVICES_SIGNATURE: u64 = 0x5652_4553_544e_5552;

/// `EFI_RESET_SYSTEM` — the MS x64 (`efiapi`) firmware entry point.
type EfiResetSystemFn = unsafe extern "efiapi" fn(
    reset_type: u32,
    reset_status: usize,
    data_size: usize,
    reset_data: *const c_void,
);

/// Invoke `EFI_RUNTIME_SERVICES.ResetSystem` through the `EFI_SYSTEM_TABLE`
/// at virtual address `system_table`.
///
/// On conforming firmware this resets / powers off and never returns; it
/// returns only if the firmware ignored the request (e.g. `EfiResetShutdown`
/// unsupported) so a fallback can run. Each pointer is validated — non-null,
/// 8-aligned, and the `"RUNTSERV"` signature — before use, so a malformed or
/// zeroed table (including `system_table == 0` for a non-UEFI boot) degrades
/// to a no-op return.
///
/// # Preconditions
///
/// `system_table`, its `RuntimeServices` table, and the `ResetSystem` code
/// must be mapped (executable) in the active address space — established by
/// `boot::uefi_runtime`. The firmware call uses the `efiapi` (MS x64) ABI.
pub fn reset_system(system_table: u64, reset_type: EfiResetType) {
    if system_table == 0 || system_table & 0x7 != 0 {
        return;
    }
    // SAFETY: per the precondition the runtime regions are mapped; the
    // field offsets are spec-fixed (UEFI 2.x). Each pointer is validated
    // (non-null, aligned, expected signature) before it is dereferenced
    // or called, so a malformed table degrades to a no-op return.
    unsafe {
        let rs_ptr = core::ptr::read_unaligned(
            (system_table as *const u8).add(SYSTEM_TABLE_RUNTIME_SERVICES) as *const u64,
        );
        if rs_ptr == 0 || rs_ptr & 0x7 != 0 {
            return;
        }
        // Validate the runtime-services table signature before trusting
        // any function pointer inside it.
        let signature = core::ptr::read_unaligned(rs_ptr as *const u64);
        if signature != RUNTIME_SERVICES_SIGNATURE {
            return;
        }
        let reset_ptr = core::ptr::read_unaligned(
            (rs_ptr as *const u8).add(RUNTIME_SERVICES_RESET_SYSTEM) as *const u64,
        );
        if reset_ptr == 0 {
            return;
        }
        let reset: EfiResetSystemFn = core::mem::transmute(reset_ptr);
        reset(reset_type as u32, 0, 0, core::ptr::null());
    }
}
