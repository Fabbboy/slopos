use slopos_arch::cpu::cpuid::CPUID_LEAF_FEATURES;

pub fn read_bsp_apic_id() -> u32 {
    let (_, ebx, _, _) = slopos_arch::cpu::cpuid(CPUID_LEAF_FEATURES);
    ((ebx >> 24) & 0xFF) as u32
}
