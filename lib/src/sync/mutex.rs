use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

use super::level::{Level, Lower};
use super::token::LockToken;
use crate::cpu;

pub struct Mutex<L: Level, T> {
    inner: spin::Mutex<T>,
    _level: PhantomData<L>,
}

unsafe impl<L: Level, T: Send> Send for Mutex<L, T> {}
unsafe impl<L: Level, T: Send> Sync for Mutex<L, T> {}

impl<L: Level, T> Mutex<L, T> {
    #[inline]
    pub const fn new(data: T) -> Self {
        Self {
            inner: spin::Mutex::new(data),
            _level: PhantomData,
        }
    }

    #[inline]
    pub fn lock<'a, LP: Lower<L> + 'a>(
        &'a self,
        _token: LockToken<'a, LP>,
    ) -> MutexGuard<'a, L, T> {
        let saved_flags = read_rflags();
        cpu::disable_interrupts();

        let inner = self.inner.lock();

        MutexGuard {
            inner,
            saved_flags,
            _token: LockToken::new(),
        }
    }

    #[inline]
    pub fn try_lock<'a, LP: Lower<L> + 'a>(
        &'a self,
        _token: LockToken<'a, LP>,
    ) -> Option<MutexGuard<'a, L, T>> {
        let saved_flags = read_rflags();
        cpu::disable_interrupts();

        match self.inner.try_lock() {
            Some(inner) => Some(MutexGuard {
                inner,
                saved_flags,
                _token: LockToken::new(),
            }),
            None => {
                if saved_flags & (1 << 9) != 0 {
                    cpu::enable_interrupts();
                }
                None
            }
        }
    }
}

pub struct MutexGuard<'a, L: Level, T> {
    inner: spin::MutexGuard<'a, T>,
    saved_flags: u64,
    _token: LockToken<'a, L>,
}

impl<'a, L: Level, T> MutexGuard<'a, L, T> {
    #[inline]
    pub fn token(&self) -> LockToken<'_, L> {
        LockToken::new()
    }
}

impl<'a, L: Level, T> Deref for MutexGuard<'a, L, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<'a, L: Level, T> DerefMut for MutexGuard<'a, L, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<'a, L: Level, T> Drop for MutexGuard<'a, L, T> {
    #[inline]
    fn drop(&mut self) {
        if self.saved_flags & (1 << 9) != 0 {
            cpu::enable_interrupts();
        }
    }
}

#[inline(always)]
fn read_rflags() -> u64 {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags));
    }
    flags
}
