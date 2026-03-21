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
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

impl Default for InitFlag {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl Send for InitFlag {}
unsafe impl Sync for InitFlag {}
