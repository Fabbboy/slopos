#[repr(C)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl InterruptFrame {
    /// Borrow an [`InterruptFrame`] from a pointer published by the
    /// IDT entry trampoline. Returns `None` for null; otherwise wraps
    /// the raw deref so callers stay in safe Rust.
    ///
    /// The lifetime of the returned reference is bounded by the
    /// caller's frame — typically the duration of an exception
    /// handler invocation, where the trampoline guarantees the frame
    /// remains valid until the handler returns.
    #[inline]
    pub fn from_ptr<'a>(ptr: *const InterruptFrame) -> Option<&'a InterruptFrame> {
        if ptr.is_null() {
            None
        } else {
            // SAFETY: caller's IDT trampoline keeps the frame alive
            // for the duration of the handler; non-null was just
            // checked.
            Some(unsafe { &*ptr })
        }
    }

    /// Mutable variant of [`InterruptFrame::from_ptr`].
    #[inline]
    pub fn from_ptr_mut<'a>(ptr: *mut InterruptFrame) -> Option<&'a mut InterruptFrame> {
        if ptr.is_null() {
            None
        } else {
            // SAFETY: as `from_ptr` — unique handler invocation
            // implies no aliased &mut.
            Some(unsafe { &mut *ptr })
        }
    }
}
