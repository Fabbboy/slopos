/// Fill `bytes` with cryptographically secure random data from the kernel.
pub fn fill_bytes(bytes: &mut [u8]) {
    const SYS_GETRANDOM: u64 = 12;

    let mut filled = 0usize;
    while filled < bytes.len() {
        let ret: u64;
        let remaining = bytes.len() - filled;
        let ptr = unsafe { bytes.as_mut_ptr().add(filled) };
        unsafe {
            core::arch::asm!(
                "syscall",
                in("rax") SYS_GETRANDOM,
                in("rdi") ptr as u64,
                in("rsi") remaining as u64,
                in("rdx") 0u64, // flags
                lateout("rax") ret,
                out("rcx") _,
                out("r11") _,
                options(nostack),
            );
        }
        let n = ret as i64;
        if n <= 0 {
            // Should never happen with a seeded CSPRNG, but don't loop forever.
            break;
        }
        filled += n as usize;
    }
}
