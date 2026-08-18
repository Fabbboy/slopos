#![allow(unsafe_op_in_unsafe_fn)]
#![allow(improper_ctypes)]

//! The only functions needing `extern "C"` linkage: those called from assembly.

slopos_ostd::extern_c_entry! {
    /// Entry point called from limine_entry.s
    pub fn kernel_main() {
        crate::early_init::kernel_main_impl();
    }
}

slopos_ostd::extern_c_entry! {
    pub fn common_exception_handler(frame: *mut slopos_arch::InterruptFrame) {
        crate::idt::common_exception_handler_impl(frame);
    }
}

slopos_ostd::extern_c_entry! {
    /// Called from ISR assembly when the IRET frame's CS is neither 0x08 nor
    /// 0x23. `iret_frame` points at the 5-word frame [RIP, CS, RFLAGS, RSP, SS];
    /// panicking here beats a triple fault from a bad IRETQ.
    pub fn isr_iret_frame_corrupt(iret_frame: *const u64) -> ! {
        crate::idt::handle_corrupt_iret_frame(iret_frame)
    }
}
