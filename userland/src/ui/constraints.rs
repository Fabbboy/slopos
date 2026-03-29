use slopos_abi::damage::DamageRect;

/// Pixel dimensions (integer, matching DrawBuffer coordinate space).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Size {
    pub width: i32,
    pub height: i32,
}

impl Size {
    pub const ZERO: Self = Self {
        width: 0,
        height: 0,
    };

    pub const fn new(width: i32, height: i32) -> Self {
        Self { width, height }
    }
}

/// Position + size in parent-local coordinates.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const ZERO: Self = Self {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };

    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }

    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.width).min(other.x + other.width);
        let y1 = (self.y + self.height).min(other.y + other.height);
        if x1 > x0 && y1 > y0 {
            Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
        } else {
            None
        }
    }

    pub fn to_damage_rect(&self) -> DamageRect {
        DamageRect {
            x0: self.x,
            y0: self.y,
            x1: self.x + self.width,
            y1: self.y + self.height,
        }
    }
}

/// Edge insets for padding.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EdgeInsets {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

impl EdgeInsets {
    pub const ZERO: Self = Self {
        top: 0,
        right: 0,
        bottom: 0,
        left: 0,
    };

    pub const fn all(v: i32) -> Self {
        Self {
            top: v,
            right: v,
            bottom: v,
            left: v,
        }
    }

    pub const fn symmetric(horizontal: i32, vertical: i32) -> Self {
        Self {
            top: vertical,
            right: horizontal,
            bottom: vertical,
            left: horizontal,
        }
    }

    pub const fn new(top: i32, right: i32, bottom: i32, left: i32) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    pub const fn horizontal(&self) -> i32 {
        self.left + self.right
    }

    pub const fn vertical(&self) -> i32 {
        self.top + self.bottom
    }
}

/// Constraint box passed top-down during the measure phase.
///
/// Uses `i32` for pixel-perfect layout matching the DrawBuffer coordinate space.
/// `max_width` / `max_height` of `i32::MAX` represents unbounded (e.g. inside a scroll view).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BoxConstraints {
    pub min_width: i32,
    pub max_width: i32,
    pub min_height: i32,
    pub max_height: i32,
}

impl BoxConstraints {
    pub const UNBOUNDED: Self = Self {
        min_width: 0,
        max_width: i32::MAX,
        min_height: 0,
        max_height: i32::MAX,
    };

    pub fn tight(size: Size) -> Self {
        Self {
            min_width: size.width,
            max_width: size.width,
            min_height: size.height,
            max_height: size.height,
        }
    }

    pub fn tight_width(width: i32) -> Self {
        Self {
            min_width: width,
            max_width: width,
            min_height: 0,
            max_height: i32::MAX,
        }
    }

    pub fn loose(max: Size) -> Self {
        Self {
            min_width: 0,
            max_width: max.width,
            min_height: 0,
            max_height: max.height,
        }
    }

    /// Clamp a size to satisfy these constraints.
    pub fn constrain(&self, size: Size) -> Size {
        Size {
            width: size.width.clamp(self.min_width, self.max_width),
            height: size.height.clamp(self.min_height, self.max_height),
        }
    }

    /// Loosen: keep max, set min to 0.
    pub fn loosen(&self) -> Self {
        Self {
            min_width: 0,
            max_width: self.max_width,
            min_height: 0,
            max_height: self.max_height,
        }
    }

    /// Deflate by edge insets (e.g. for padding).
    pub fn deflate(&self, insets: EdgeInsets) -> Self {
        let h = insets.horizontal();
        let v = insets.vertical();
        Self {
            min_width: (self.min_width - h).max(0),
            max_width: if self.max_width == i32::MAX {
                i32::MAX
            } else {
                (self.max_width - h).max(0)
            },
            min_height: (self.min_height - v).max(0),
            max_height: if self.max_height == i32::MAX {
                i32::MAX
            } else {
                (self.max_height - v).max(0)
            },
        }
    }

    pub fn is_tight(&self) -> bool {
        self.min_width == self.max_width && self.min_height == self.max_height
    }

    /// Return maximum available size (capped at a reasonable value for unbounded axes).
    pub fn max_size(&self) -> Size {
        Size::new(self.max_width, self.max_height)
    }
}

/// How a widget (or slot in a layout container) participates in space distribution.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SizePolicy {
    /// Use exactly the intrinsic content size. Do not grow or shrink.
    Fixed,
    /// Use intrinsic content size as preferred, but may shrink to min.
    Shrink,
    /// Expand to fill available space. Weight controls proportional share.
    Expand { weight: u16 },
}

impl Default for SizePolicy {
    fn default() -> Self {
        Self::Fixed
    }
}

/// Declarative sizing intent. Replaces raw i32 in user-facing APIs.
///
/// Use `Length::Px(n)` for fixed pixel sizes and `Length::Fill(weight)` for
/// proportional space distribution. This prevents the class of bugs where
/// unbounded i32::MAX values leak into widget measurement.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Length {
    /// Fixed pixel size.
    Px(i32),
    /// Fill remaining space proportionally. Weight 0 = shrink to content.
    Fill(u16),
}

impl Length {
    /// Shrink to intrinsic content size.
    pub const SHRINK: Self = Length::Fill(0);
    /// Fill all remaining space (weight 1).
    pub const FILL: Self = Length::Fill(1);

    /// Resolve this length against a constraint range, returning a pixel value.
    pub fn resolve(&self, min: i32, max: i32) -> i32 {
        match self {
            Length::Px(px) => (*px).clamp(min, max),
            Length::Fill(0) => min, // shrink: use minimum
            Length::Fill(_) => max, // fill: use maximum (parent distributes)
        }
    }

    /// Whether this length is flexible (Fill with weight > 0).
    pub fn is_fill(&self) -> bool {
        matches!(self, Length::Fill(w) if *w > 0)
    }

    /// The flex weight (0 for Px and Fill(0)).
    pub fn flex_weight(&self) -> u16 {
        match self {
            Length::Fill(w) => *w,
            Length::Px(_) => 0,
        }
    }
}

impl Default for Length {
    fn default() -> Self {
        Length::SHRINK
    }
}

/// Cross-axis alignment for stack layouts.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum CrossAxisAlignment {
    #[default]
    Start,
    Center,
    End,
    Stretch,
}

/// Text alignment within a label or text widget.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum TextAlignment {
    #[default]
    Start,
    Center,
    End,
}

/// Axis orientation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Orientation {
    Horizontal,
    Vertical,
}

/// Scroll direction for scroll views.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ScrollDirection {
    #[default]
    Vertical,
    Horizontal,
    Both,
}

/// Scrollbar visibility mode.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ScrollbarVisibility {
    Always,
    #[default]
    WhenNeeded,
    Never,
}

/// Image scaling mode.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ImageScale {
    /// Scale to fit within constraints preserving aspect ratio.
    #[default]
    Fit,
    /// Stretch to fill constraints.
    Fill,
    /// Display at source dimensions.
    None,
}
