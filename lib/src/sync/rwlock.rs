use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

use super::level::{Level, Lower};
use super::token::LockToken;
use crate::cpu;

pub struct RwLock<L: Level, T> {
    inner: spin::RwLock<T>,
    _level: PhantomData<L>,
}

unsafe impl<L: Level, T: Send> Send for RwLock<L, T> {}
unsafe impl<L: Level, T: Send + Sync> Sync for RwLock<L, T> {}

impl<L: Level, T> RwLock<L, T> {
    #[inline]
    pub const fn new(data: T) -> Self {
        Self {
            inner: spin::RwLock::new(data),
            _level: PhantomData,
        }
    }

    #[inline]
    pub fn read<'a, LP: Lower<L> + 'a>(
        &'a self,
        _token: LockToken<'a, LP>,
    ) -> RwLockReadGuard<'a, L, T> {
        let saved_flags = cpu::save_flags_cli();

        let inner = self.inner.read();

        RwLockReadGuard {
            inner,
            saved_flags,
            _token: LockToken::new(),
        }
    }

    #[inline]
    pub fn try_read<'a, LP: Lower<L> + 'a>(
        &'a self,
        _token: LockToken<'a, LP>,
    ) -> Option<RwLockReadGuard<'a, L, T>> {
        let saved_flags = cpu::save_flags_cli();

        match self.inner.try_read() {
            Some(inner) => Some(RwLockReadGuard {
                inner,
                saved_flags,
                _token: LockToken::new(),
            }),
            None => {
                cpu::restore_flags(saved_flags);
                None
            }
        }
    }

    #[inline]
    pub fn write<'a, LP: Lower<L> + 'a>(
        &'a self,
        _token: LockToken<'a, LP>,
    ) -> RwLockWriteGuard<'a, L, T> {
        let saved_flags = cpu::save_flags_cli();

        let inner = self.inner.write();

        RwLockWriteGuard {
            inner,
            saved_flags,
            _token: LockToken::new(),
        }
    }

    #[inline]
    pub fn try_write<'a, LP: Lower<L> + 'a>(
        &'a self,
        _token: LockToken<'a, LP>,
    ) -> Option<RwLockWriteGuard<'a, L, T>> {
        let saved_flags = cpu::save_flags_cli();

        match self.inner.try_write() {
            Some(inner) => Some(RwLockWriteGuard {
                inner,
                saved_flags,
                _token: LockToken::new(),
            }),
            None => {
                cpu::restore_flags(saved_flags);
                None
            }
        }
    }
}

pub struct RwLockReadGuard<'a, L: Level, T> {
    inner: spin::RwLockReadGuard<'a, T>,
    saved_flags: u64,
    _token: LockToken<'a, L>,
}

impl<'a, L: Level, T> RwLockReadGuard<'a, L, T> {
    #[inline]
    pub fn token(&self) -> LockToken<'_, L> {
        LockToken::new()
    }
}

impl<'a, L: Level, T> Deref for RwLockReadGuard<'a, L, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<'a, L: Level, T> Drop for RwLockReadGuard<'a, L, T> {
    #[inline]
    fn drop(&mut self) {
        cpu::restore_flags(self.saved_flags);
    }
}

pub struct RwLockWriteGuard<'a, L: Level, T> {
    inner: spin::RwLockWriteGuard<'a, T>,
    saved_flags: u64,
    _token: LockToken<'a, L>,
}

impl<'a, L: Level, T> RwLockWriteGuard<'a, L, T> {
    #[inline]
    pub fn token(&self) -> LockToken<'_, L> {
        LockToken::new()
    }
}

impl<'a, L: Level, T> Deref for RwLockWriteGuard<'a, L, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<'a, L: Level, T> DerefMut for RwLockWriteGuard<'a, L, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<'a, L: Level, T> Drop for RwLockWriteGuard<'a, L, T> {
    #[inline]
    fn drop(&mut self) {
        cpu::restore_flags(self.saved_flags);
    }
}
