/// Dirty flags for incremental update scheduling.
#[derive(Copy, Clone, Debug, Default)]
pub struct DirtyFlags(u8);

impl DirtyFlags {
    pub const NONE: Self = Self(0);
    pub const NEEDS_MEASURE: Self = Self(0b01);
    pub const NEEDS_PAINT: Self = Self(0b10);
    pub const ALL: Self = Self(0b11);

    pub const fn needs_measure(self) -> bool {
        self.0 & Self::NEEDS_MEASURE.0 != 0
    }

    pub const fn needs_paint(self) -> bool {
        self.0 & Self::NEEDS_PAINT.0 != 0
    }

    pub const fn is_clean(self) -> bool {
        self.0 == 0
    }

    pub fn set_needs_measure(&mut self) {
        self.0 |= Self::NEEDS_MEASURE.0;
    }

    pub fn set_needs_paint(&mut self) {
        self.0 |= Self::NEEDS_PAINT.0;
    }

    pub fn clear(&mut self) {
        self.0 = 0;
    }

    pub fn merge(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}
