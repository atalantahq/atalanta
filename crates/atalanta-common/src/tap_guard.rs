// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! The part of the tap guard every platform shares: which points of a
//! target to try, in which order (review 06 #4).
//!
//! What covers a point is platform knowledge and stays in each driver:
//! Android asks the window list (front first, the keyboard, system windows,
//! another app's window), iOS the tree's bars, keyboard and modal layers
//! plus the status-bar rule, the web the page's own hit test (in its
//! injected script). Each driver hands its "is this point covered" test to
//! [`free_point`] and keeps its own words for what covered it.

use crate::Rect;

/// Fractions (percent) of the target tried on each axis after its centre,
/// middle out: a partly covered target is tapped as near its middle as
/// what is left allows.
const GRID: [i64; 5] = [50, 25, 75, 10, 90];

/// The points of `rect` a tap tries: its centre, then a 5×5 grid, rows
/// from the middle out. Computed wide and clamped, so hostile bounds can't
/// overflow.
pub fn candidate_points(rect: &Rect) -> impl Iterator<Item = (i32, i32)> + '_ {
    let at = |o: i32, len: i32, pct: i64| {
        (i64::from(o) + i64::from(len) * pct / 100).clamp(i64::from(i32::MIN), i64::from(i32::MAX))
            as i32
    };
    std::iter::once(rect.center()).chain(GRID.iter().flat_map(move |&fy| {
        GRID.iter()
            .map(move |&fx| (at(rect.x, rect.width, fx), at(rect.y, rect.height, fy)))
    }))
}

/// The first point of `preferred` and then [`candidate_points`] of `rect`
/// that `covered` says is free, or `None` when every one is covered.
pub fn free_point(
    rect: &Rect,
    preferred: &[(i32, i32)],
    covered: impl Fn((i32, i32)) -> bool,
) -> Option<(i32, i32)> {
    preferred
        .iter()
        .copied()
        .chain(candidate_points(rect))
        .find(|&p| !covered(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn the_centre_comes_first_then_the_grid_from_the_middle_out() {
        let r = rect(0, 0, 100, 100);
        let pts: Vec<_> = candidate_points(&r).collect();
        assert_eq!(pts.len(), 26);
        assert_eq!(pts[0], (50, 50));
        assert_eq!(pts[1], (50, 50));
        assert_eq!(pts[2], (25, 50));
        assert_eq!(pts[25], (90, 90));
    }

    #[test]
    fn a_covered_bottom_half_leaves_the_top() {
        let r = rect(0, 100, 200, 100);
        let p = free_point(&r, &[], |(_, y)| y >= 150).unwrap();
        assert_eq!(p, (100, 125));
        assert_eq!(free_point(&r, &[], |_| true), None);
        // A preferred point is tried first.
        assert_eq!(free_point(&r, &[(190, 110)], |_| false), Some((190, 110)));
    }

    #[test]
    fn hostile_bounds_never_overflow() {
        let r = rect(i32::MAX - 1, i32::MAX - 1, i32::MAX, i32::MAX);
        assert!(candidate_points(&r).all(|(x, y)| x == i32::MAX && y == i32::MAX));
    }
}
