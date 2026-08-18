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

// SAFETY: every field is a `u64` — a primitive whose all-zero bit
// pattern is a valid value. No references, pointers, niche-constrained
// enums, or `bool` fields. `#[repr(C)]` pins the layout so the impl
// stays well-formed under field reorder.
unsafe impl crate::mm::init::Zeroable for InterruptFrame {}

impl InterruptFrame {
    /// Borrow an [`InterruptFrame`] from a pointer published by the IDT entry
    /// trampoline; `None` for null.
    ///
    /// The lifetime is anchored rather than caller-chosen because the frame is
    /// alive for exactly one handler invocation: a lifetime the caller names is
    /// one it could name twice, and two `&mut` to one frame is aliasing UB.
    #[inline]
    pub fn from_ptr<'a, A: ?Sized>(
        anchor: &'a A,
        ptr: *const InterruptFrame,
    ) -> Option<&'a InterruptFrame> {
        let _ = anchor;
        if ptr.is_null() {
            None
        } else {
            // SAFETY: caller's IDT trampoline keeps the frame alive
            // for the duration of the handler; non-null was just
            // checked.
            Some(unsafe { &*ptr })
        }
    }

    /// Mutable variant of [`InterruptFrame::from_ptr`]. Takes `&mut A`, so the
    /// exclusivity of the frame is the exclusivity of the anchor.
    #[inline]
    pub fn from_ptr_mut<'a, A: ?Sized>(
        anchor: &'a mut A,
        ptr: *mut InterruptFrame,
    ) -> Option<&'a mut InterruptFrame> {
        let _ = anchor;
        if ptr.is_null() {
            None
        } else {
            // SAFETY: as `from_ptr` — unique handler invocation
            // implies no aliased &mut.
            Some(unsafe { &mut *ptr })
        }
    }
}
