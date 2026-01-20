use core::marker::PhantomData;

use super::level::{Level, Lower, L0};

pub struct LockToken<'a, L: Level>(PhantomData<&'a mut L>);

impl<'a, L: Level> LockToken<'a, L> {
    #[inline]
    pub(super) fn new() -> Self {
        LockToken(PhantomData)
    }

    #[inline]
    pub fn downgrade<LP: Lower<L>>(self) -> LockToken<'a, L> {
        let _ = self;
        LockToken(PhantomData)
    }
}

pub struct CleanLockToken(());

impl CleanLockToken {
    #[inline]
    pub unsafe fn new() -> Self {
        CleanLockToken(())
    }

    #[inline]
    pub fn token(&mut self) -> LockToken<'_, L0> {
        LockToken::new()
    }
}
