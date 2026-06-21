//! Rectangle-set region algebra for occlusion culling and damage tracking.
//!
//! A [`Region`] is a set of inclusive integer rectangles supporting the three
//! operations the occlusion pass needs: union ([`push`](Region::push)),
//! intersection with a clip rect ([`intersect_rect`](Region::intersect_rect)),
//! and **subtraction** of an occluding rect ([`subtract`](Region::subtract)).
//! Subtracting each opaque window's box front-to-back carves the covered area
//! out of the still-visible region.
//!
//! `subtract` fragments each stored rect into up to four disjoint remainder rects
//! (left/right/above/below the cut), so a region is not necessarily disjoint
//! after a `push`; every `subtract`/`intersect_rect` result, however, is exact —
//! it covers precisely the set-operation area with no pixel double-counted.

use slopos_abi::damage::DamageRect;
use std::vec::Vec;

/// A set of inclusive integer rectangles.
#[derive(Clone, Default)]
pub struct Region {
    rects: Vec<DamageRect>,
}

impl Region {
    /// An empty region.
    pub fn new() -> Self {
        Self { rects: Vec::new() }
    }

    /// A region containing a single rect (ignored if invalid/empty).
    pub fn from_rect(rect: DamageRect) -> Self {
        let mut r = Self::new();
        r.push(rect);
        r
    }

    /// A region covering the whole `width`×`height` output.
    pub fn full(width: u32, height: u32) -> Self {
        Self::from_rect(DamageRect {
            x0: 0,
            y0: 0,
            x1: width as i32 - 1,
            y1: height as i32 - 1,
        })
    }

    /// Add a rect to the region, allowing overlap with existing rects.
    ///
    /// Cheap (a single push), but the region may then double-cover pixels. Use
    /// for accumulating into a region that will only ever be *subtracted* from
    /// or intersected (where overlap is harmless), never blend-painted.
    pub fn push(&mut self, rect: DamageRect) {
        if rect.is_valid() {
            self.rects.push(rect);
        }
    }

    /// Add a rect as a **disjoint** union: the new rect's area is first removed
    /// from every existing rect, so the region stays a non-overlapping cover.
    ///
    /// Damage accumulation uses this: a disjoint frame-damage region guarantees
    /// the occlusion pass never blend-paints a pixel twice.
    pub fn add(&mut self, rect: DamageRect) {
        if !rect.is_valid() {
            return;
        }
        self.subtract(&rect);
        self.rects.push(rect);
    }

    /// Add every rect of `other` to this region (set union).
    pub fn push_region(&mut self, other: &Region) {
        self.rects.extend_from_slice(&other.rects);
    }

    /// Disjoint-union an inclusive rect given by its corners. Convenience over
    /// [`add`](Region::add) for the compositor's damage helpers.
    #[inline]
    pub fn add_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.add(DamageRect { x0, y0, x1, y1 });
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    #[inline]
    pub fn clear(&mut self) {
        self.rects.clear();
    }

    #[inline]
    pub fn rects(&self) -> &[DamageRect] {
        &self.rects
    }

    /// The region clipped to `clip`: every stored rect intersected with `clip`,
    /// empties dropped. Returns the parts of `self` that lie inside `clip`.
    pub fn intersect_rect(&self, clip: &DamageRect) -> Region {
        let mut out = Region::new();
        for r in &self.rects {
            if let Some(i) = rect_intersect(r, clip) {
                out.rects.push(i);
            }
        }
        out
    }

    /// Subtract `cut` from every rect in the region (set difference).
    ///
    /// Each stored rect is replaced by the (up to four) disjoint rectangles
    /// covering the part of it **not** inside `cut`. This is the occlusion
    /// primitive: subtracting an opaque window's box removes exactly the pixels
    /// it covers from the still-visible region.
    pub fn subtract(&mut self, cut: &DamageRect) {
        if !cut.is_valid() {
            return;
        }
        // Rebuild in place: each rect either survives whole (no overlap) or is
        // shattered into its remainder pieces.
        let old = core::mem::take(&mut self.rects);
        for r in old {
            subtract_into(&r, cut, &mut self.rects);
        }
    }

    /// Coalesce down to at most `N` rects by repeatedly merging the pair whose
    /// bounding-box union has the smallest area, returning them as a fixed array
    /// + count. Hands the kernel a bounded damage list that is a superset of the
    /// precise region — never fewer pixels than were painted.
    pub fn to_bounded<const N: usize>(&self) -> ([DamageRect; N], usize) {
        let mut work: Vec<DamageRect> = self.rects.clone();
        while work.len() > N {
            // Find the pair with the smallest combined area and merge it.
            let mut best = (0usize, 1usize);
            let mut best_area = i32::MAX;
            for i in 0..work.len() {
                for j in (i + 1)..work.len() {
                    let area = work[i].combined_area(&work[j]);
                    if area < best_area {
                        best_area = area;
                        best = (i, j);
                    }
                }
            }
            let (i, j) = best;
            work[i] = work[i].union(&work[j]);
            work.swap_remove(j);
        }
        let mut out = [DamageRect::invalid(); N];
        let count = work.len().min(N);
        out[..count].copy_from_slice(&work[..count]);
        (out, count)
    }
}

/// Intersection of two inclusive rects, or `None` if disjoint.
#[inline]
fn rect_intersect(a: &DamageRect, b: &DamageRect) -> Option<DamageRect> {
    let x0 = a.x0.max(b.x0);
    let y0 = a.y0.max(b.y0);
    let x1 = a.x1.min(b.x1);
    let y1 = a.y1.min(b.y1);
    if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    }
}

/// Push the parts of `r` that are NOT covered by `cut` into `out` (up to four
/// disjoint rects). If `r` and `cut` are disjoint, `r` is pushed unchanged.
fn subtract_into(r: &DamageRect, cut: &DamageRect, out: &mut Vec<DamageRect>) {
    let Some(overlap) = rect_intersect(r, cut) else {
        out.push(*r);
        return;
    };
    // Top strip: rows above the overlap.
    if r.y0 < overlap.y0 {
        out.push(DamageRect {
            x0: r.x0,
            y0: r.y0,
            x1: r.x1,
            y1: overlap.y0 - 1,
        });
    }
    // Bottom strip: rows below the overlap.
    if r.y1 > overlap.y1 {
        out.push(DamageRect {
            x0: r.x0,
            y0: overlap.y1 + 1,
            x1: r.x1,
            y1: r.y1,
        });
    }
    // Left strip: columns left of the overlap, within the overlap's row span.
    if r.x0 < overlap.x0 {
        out.push(DamageRect {
            x0: r.x0,
            y0: overlap.y0,
            x1: overlap.x0 - 1,
            y1: overlap.y1,
        });
    }
    // Right strip: columns right of the overlap, within the overlap's row span.
    if r.x1 > overlap.x1 {
        out.push(DamageRect {
            x0: overlap.x1 + 1,
            y0: overlap.y0,
            x1: r.x1,
            y1: overlap.y1,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(region: &Region) -> i32 {
        region.rects().iter().map(|r| r.area()).sum()
    }

    fn covers(region: &Region, x: i32, y: i32) -> bool {
        region.rects().iter().any(|r| r.contains(x, y))
    }

    #[test]
    fn subtract_hole_in_middle_makes_four_disjoint_rects() {
        let mut r = Region::from_rect(DamageRect {
            x0: 0,
            y0: 0,
            x1: 9,
            y1: 9,
        });
        r.subtract(&DamageRect {
            x0: 3,
            y0: 3,
            x1: 6,
            y1: 6,
        });
        // 100 total minus the 4x4 hole = 84 pixels, no double-count.
        assert_eq!(area(&r), 100 - 16);
        // Hole interior is gone; the ring is present.
        assert!(!covers(&r, 4, 4));
        assert!(covers(&r, 0, 0));
        assert!(covers(&r, 9, 9));
        assert!(covers(&r, 2, 4));
        assert!(covers(&r, 7, 4));
    }

    #[test]
    fn subtract_disjoint_keeps_rect() {
        let mut r = Region::from_rect(DamageRect {
            x0: 0,
            y0: 0,
            x1: 4,
            y1: 4,
        });
        r.subtract(&DamageRect {
            x0: 10,
            y0: 10,
            x1: 20,
            y1: 20,
        });
        assert_eq!(area(&r), 25);
    }

    #[test]
    fn subtract_full_cover_empties() {
        let mut r = Region::from_rect(DamageRect {
            x0: 2,
            y0: 2,
            x1: 5,
            y1: 5,
        });
        r.subtract(&DamageRect {
            x0: 0,
            y0: 0,
            x1: 9,
            y1: 9,
        });
        assert!(r.is_empty());
    }

    #[test]
    fn subtract_remainder_rects_are_pairwise_disjoint() {
        // After subtracting a centered hole, no two remainder rects overlap —
        // the occlusion invariant (no pixel counted twice).
        let mut r = Region::from_rect(DamageRect {
            x0: 0,
            y0: 0,
            x1: 20,
            y1: 20,
        });
        r.subtract(&DamageRect {
            x0: 5,
            y0: 5,
            x1: 15,
            y1: 15,
        });
        let rects = r.rects();
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(
                    rect_intersect(&rects[i], &rects[j]).is_none(),
                    "remainder rects {i} and {j} overlap"
                );
            }
        }
    }

    #[test]
    fn intersect_rect_clips() {
        let r = Region::from_rect(DamageRect {
            x0: 0,
            y0: 0,
            x1: 9,
            y1: 9,
        });
        let clipped = r.intersect_rect(&DamageRect {
            x0: 5,
            y0: 5,
            x1: 20,
            y1: 20,
        });
        assert_eq!(area(&clipped), 25); // [5,5]..[9,9]
        assert!(covers(&clipped, 9, 9));
        assert!(!covers(&clipped, 4, 4));
    }

    #[test]
    fn to_bounded_merges_down_and_is_superset() {
        let mut r = Region::new();
        for i in 0..10 {
            r.push(DamageRect {
                x0: i * 4,
                y0: 0,
                x1: i * 4 + 1,
                y1: 1,
            });
        }
        let (arr, n) = r.to_bounded::<4>();
        assert!(n <= 4);
        // Every original rect's corner is still covered by some merged rect.
        for i in 0..10 {
            let (px, py) = (i * 4, 0);
            assert!(
                arr[..n].iter().any(|m| m.contains(px, py)),
                "merged region must be a superset"
            );
        }
    }

    #[test]
    fn add_keeps_region_disjoint() {
        let mut r = Region::new();
        r.add(DamageRect {
            x0: 0,
            y0: 0,
            x1: 9,
            y1: 9,
        });
        r.add(DamageRect {
            x0: 5,
            y0: 5,
            x1: 14,
            y1: 14,
        }); // overlaps the first
        // Union area = 100 + 100 - 25 overlap = 175, counted exactly once.
        assert_eq!(area(&r), 175);
        let rects = r.rects();
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(
                    rect_intersect(&rects[i], &rects[j]).is_none(),
                    "add() must keep rects disjoint"
                );
            }
        }
    }

    #[test]
    fn occlusion_pass_carves_opaque_window() {
        // frame damage = whole 100x100 output; one opaque window covers [20,20]..[60,60].
        let mut uncovered = Region::full(100, 100);
        uncovered.subtract(&DamageRect {
            x0: 20,
            y0: 20,
            x1: 60,
            y1: 60,
        });
        // Background visible everywhere except under the opaque window.
        assert!(!covers(&uncovered, 40, 40));
        assert!(covers(&uncovered, 0, 0));
        assert!(covers(&uncovered, 99, 99));
        assert_eq!(area(&uncovered), 100 * 100 - 41 * 41);
    }
}
