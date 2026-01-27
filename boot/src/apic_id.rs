pub fn read_bsp_apic_id() -> u32 {
    let (_, ebx, _, _) = slopos_lib::cpu::cpuid(1);
    ((ebx >> 24) & 0xFF) as u32
}
