//! Local APIC MMIO register offsets and control flags. The IA32_APIC_BASE MSR
//! definitions live in `slopos_arch::cpu::apic_msr` instead, for subsystems that
//! need the APIC base without touching LAPIC registers.

pub(crate) const LAPIC_ID: u32 = 0x020;
pub(crate) const LAPIC_VERSION: u32 = 0x030;
pub(crate) const LAPIC_EOI: u32 = 0x0B0;
pub(crate) const LAPIC_SPURIOUS: u32 = 0x0F0;
pub(crate) const LAPIC_ESR: u32 = 0x280;
pub(crate) const LAPIC_ICR_LOW: u32 = 0x300;
pub(crate) const LAPIC_ICR_HIGH: u32 = 0x310;
pub(crate) const LAPIC_LVT_TIMER: u32 = 0x320;
pub(crate) const LAPIC_LVT_PERFCNT: u32 = 0x340;
pub(crate) const LAPIC_LVT_LINT0: u32 = 0x350;
pub(crate) const LAPIC_LVT_LINT1: u32 = 0x360;
pub(crate) const LAPIC_LVT_ERROR: u32 = 0x370;
pub(crate) const LAPIC_TIMER_ICR: u32 = 0x380;
pub(crate) const LAPIC_TIMER_CCR: u32 = 0x390;
pub(crate) const LAPIC_TIMER_DCR: u32 = 0x3E0;

pub(crate) const LAPIC_SPURIOUS_ENABLE: u32 = 1 << 8;
pub(crate) const LAPIC_LVT_MASKED: u32 = 1 << 16;

pub(crate) const LAPIC_TIMER_ONESHOT: u32 = 0x0000_0000;
pub(crate) const LAPIC_TIMER_PERIODIC: u32 = 0x0002_0000;
pub(crate) const LAPIC_TIMER_DIV_16: u32 = 0x3;

pub(crate) const LAPIC_ICR_DELIVERY_FIXED: u32 = 0 << 8;
pub(crate) const LAPIC_ICR_DEST_PHYSICAL: u32 = 0 << 11;
pub(crate) const LAPIC_ICR_LEVEL_ASSERT: u32 = 1 << 14;
pub(crate) const LAPIC_ICR_TRIGGER_EDGE: u32 = 0 << 15;
pub(crate) const LAPIC_ICR_DEST_BROADCAST: u32 = 0xFF << 24;
pub(crate) const LAPIC_ICR_DELIVERY_STATUS: u32 = 1 << 12;
