use core::sync::atomic::{AtomicBool, Ordering};

#[repr(transparent)]
pub struct InitFlag {
    flag: AtomicBool,
}

impl InitFlag {
    #[inline]
    pub const fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
        }
    }

    #[inline]
    pub fn init_once(&self) -> bool {
        !self.flag.swap(true, Ordering::SeqCst)
    }

    #[inline]
    pub fn claim(&self) -> bool {
        self.init_once()
    }

    #[inline]
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_set_relaxed(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn mark_set(&self) {
        self.flag.store(true, Ordering::Release);
    }

    #[inline]
    pub fn reset(&self) {
        self.flag.store(false, Ordering::Release);
    }
}

impl Default for InitFlag {
    fn default() -> Self {
        Self::new()
    }
}
