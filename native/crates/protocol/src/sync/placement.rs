//! A detached window's placement as the account keeps it (plans/09 §3.4, 09a
//! §3.1, §5.4, §5.5): its rectangle as shares of a monitor's work area, so a
//! window that covered the right half of a laptop screen covers the right
//! half of whatever screen the next computer has. Pure arithmetic; the app
//! reads the monitors and the window's rectangle from the OS and applies the
//! answer.
//!
//! Every rectangle here is in one coordinate space, the one the OS reports
//! monitors and windows in together: physical pixels on Windows and Linux,
//! logical points on macOS (09 §7: tao scales monitor and window positions
//! by different factors there). Only the ratios are kept, so the unit never
//! reaches the account.
//!
//! **Monitors are numbered** the same way on every computer: the primary is
//! `0`, then the others left to right, then top to bottom. A window saved on
//! monitor 2 of three opens on monitor 2 where there are three, and on the
//! primary where there are fewer.

use super::tabs::{Pos, Size, FRACTION_ONE, MAX_MONITOR};

/// A rectangle: top-left corner and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    fn right(self) -> i64 {
        i64::from(self.x) + i64::from(self.w)
    }

    fn bottom(self) -> i64 {
        i64::from(self.y) + i64::from(self.h)
    }

    fn centre(self) -> (i64, i64) {
        (
            i64::from(self.x) + i64::from(self.w) / 2,
            i64::from(self.y) + i64::from(self.h) / 2,
        )
    }

    /// The area `self` and `other` share.
    fn overlap(self, other: Rect) -> i64 {
        let w = self.right().min(other.right()) - i64::from(self.x.max(other.x));
        let h = self.bottom().min(other.bottom()) - i64::from(self.y.max(other.y));
        if w > 0 && h > 0 {
            w * h
        } else {
            0
        }
    }
}

/// One monitor as the OS reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    /// The whole monitor: what numbering goes by, so moving or hiding a
    /// taskbar never renumbers the screens.
    pub bounds: Rect,
    /// The work area: the monitor less the taskbar, dock or panels; what the
    /// shares are of.
    pub work: Rect,
    pub primary: bool,
}

/// What is saved for a window in its normal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub size: Size,
    pub pos: Pos,
}

/// Indices into `monitors` in placement order: the primary first, then the
/// rest left to right, then top to bottom. Without a primary (GDK on Wayland
/// may not name one) the first listed counts as primary; with several, the
/// first of them. At most `MAX_MONITOR + 1` are numbered.
pub fn monitor_order(monitors: &[Monitor]) -> Vec<usize> {
    // A monitor with no work area (reported mid-change) holds no window.
    let usable: Vec<usize> = (0..monitors.len())
        .filter(|&i| monitors[i].work.w > 0 && monitors[i].work.h > 0)
        .collect();
    let Some(&first) = usable.first() else {
        return Vec::new();
    };
    let primary = usable
        .iter()
        .copied()
        .find(|&i| monitors[i].primary)
        .unwrap_or(first);
    let mut rest: Vec<usize> = usable.into_iter().filter(|&i| i != primary).collect();
    rest.sort_by_key(|&i| (monitors[i].bounds.x, monitors[i].bounds.y, i));
    let mut order = Vec::with_capacity(monitors.len());
    order.push(primary);
    order.extend(rest);
    order.truncate(usize::from(MAX_MONITOR) + 1);
    order
}

/// The monitor `window` is on: the one it overlaps most; when it overlaps
/// none (it was dragged off every screen), the one whose work area's centre
/// is nearest its own. An index into `order`.
fn home(window: Rect, monitors: &[Monitor], order: &[usize]) -> Option<usize> {
    let best = order
        .iter()
        .enumerate()
        .max_by_key(|(n, &i)| (monitors[i].work.overlap(window), std::cmp::Reverse(*n)))?;
    if monitors[*best.1].work.overlap(window) > 0 {
        return Some(best.0);
    }
    let (cx, cy) = window.centre();
    order
        .iter()
        .enumerate()
        .min_by_key(|(n, &i)| {
            let (mx, my) = monitors[i].work.centre();
            // Saturating: two far corners of the i32 plane overflow i64.
            let d = (mx - cx)
                .saturating_pow(2)
                .saturating_add((my - cy).saturating_pow(2));
            (d, *n)
        })
        .map(|(n, _)| n)
}

/// `part` of `whole` in ten-thousandths, rounded, within `lo..=FRACTION_ONE`.
fn share(part: i64, whole: u32, lo: u16) -> u16 {
    let whole = i64::from(whole.max(1));
    let n = (part * i64::from(FRACTION_ONE) + whole / 2).div_euclid(whole);
    n.clamp(i64::from(lo), i64::from(FRACTION_ONE)) as u16
}

/// `share` ten-thousandths of `whole`, rounded.
fn of(share: u16, whole: u32) -> i64 {
    (i64::from(share) * i64::from(whole) + i64::from(FRACTION_ONE) / 2) / i64::from(FRACTION_ONE)
}

/// What to save for `window` (its outer rectangle, in the normal state),
/// or `None` without monitors.
pub fn to_placement(window: Rect, monitors: &[Monitor]) -> Option<Placement> {
    let order = monitor_order(monitors);
    let n = home(window, monitors, &order)?;
    let work = monitors[order[n]].work;
    Some(Placement {
        size: Size {
            w: share(i64::from(window.w), work.w, 1),
            h: share(i64::from(window.h), work.h, 1),
        },
        pos: Pos {
            // `order` holds at most MAX_MONITOR + 1 entries.
            monitor: n as u8,
            x: share(i64::from(window.x) - i64::from(work.x), work.w, 0),
            y: share(i64::from(window.y) - i64::from(work.y), work.h, 0),
        },
    })
}

/// Where to open a detached window, or `None` without monitors (the OS
/// places it).
///
/// The monitor is `pos.monitor` where it exists, else the primary; with no
/// position at all (never saved, or saved where positions are unknown) it is
/// `unplaced_on` (an index into `monitors`, e.g. the main window's monitor),
/// else the primary — centred there. `size` falls back to `default_size`.
/// The result is at least `min` (or the whole work area, if smaller) and
/// lies wholly on the work area: a window saved half off a screen comes
/// back fully on it.
pub fn to_rect(
    size: Option<Size>,
    pos: Option<Pos>,
    monitors: &[Monitor],
    unplaced_on: Option<usize>,
    default_size: Size,
    min: (u32, u32),
) -> Option<Rect> {
    let order = monitor_order(monitors);
    let first = *order.first()?;
    let monitor = match pos {
        Some(p) => order.get(usize::from(p.monitor)).copied().unwrap_or(first),
        None => unplaced_on.filter(|i| order.contains(i)).unwrap_or(first),
    };
    let work = monitors[monitor].work;
    let size = size.unwrap_or(default_size);
    let dim = |share: u16, whole: u32, min: u32| -> u32 {
        // Within u32: at most `whole`.
        of(share, whole).clamp(i64::from(min.min(whole)), i64::from(whole)) as u32
    };
    let (w, h) = (dim(size.w, work.w, min.0), dim(size.h, work.h, min.1));
    let (x, y) = match pos {
        Some(p) => (
            i64::from(work.x) + of(p.x, work.w),
            i64::from(work.y) + of(p.y, work.h),
        ),
        None => (
            i64::from(work.x) + (i64::from(work.w) - i64::from(w)) / 2,
            i64::from(work.y) + (i64::from(work.h) - i64::from(h)) / 2,
        ),
    };
    // Onto the work area: `w <= work.w`, so the range is never empty.
    let x = saturate(x.clamp(i64::from(work.x), work.right() - i64::from(w)));
    let y = saturate(y.clamp(i64::from(work.y), work.bottom() - i64::from(h)));
    Some(Rect { x, y, w, h })
}

/// `v` as an i32, held at its limits (a work area reaching past the end of
/// the i32 plane).
fn saturate(v: i64) -> i32 {
    i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(x: i32, y: i32, w: u32, h: u32, primary: bool) -> Monitor {
        let r = Rect { x, y, w, h };
        Monitor {
            bounds: r,
            work: r,
            primary,
        }
    }

    const HALF: Size = Size { w: 5000, h: 5000 };

    #[test]
    fn a_taskbar_does_not_renumber_the_monitors() {
        // Two monitors stacked; the lower one has a taskbar on its left.
        let top = mon(0, 0, 1920, 1080, true);
        let mut lower = mon(0, 1080, 1920, 1080, false);
        lower.work = Rect {
            x: 48,
            y: 1080,
            w: 1872,
            h: 1080,
        };
        let side = mon(-1920, 500, 1920, 1080, false);
        assert_eq!(monitor_order(&[top, lower, side]), vec![0, 2, 1]);
    }

    #[test]
    fn the_primary_is_zero_then_left_to_right() {
        let ms = [
            mon(1920, 0, 1920, 1040, false),
            mon(-1280, 0, 1280, 984, false),
            mon(0, 0, 1920, 1040, true),
            mon(0, 1080, 1920, 1040, false),
        ];
        assert_eq!(monitor_order(&ms), vec![2, 1, 3, 0]);
        let none_primary = [mon(500, 0, 10, 10, false), mon(0, 0, 10, 10, false)];
        assert_eq!(monitor_order(&none_primary), vec![0, 1]);
        assert!(monitor_order(&[]).is_empty());
    }

    #[test]
    fn a_window_keeps_its_share_of_the_screen_on_another_computer() {
        // The right half of a 1920×1040 work area…
        let here = [mon(0, 0, 1920, 1040, true)];
        let window = Rect {
            x: 960,
            y: 0,
            w: 960,
            h: 1040,
        };
        let p = to_placement(window, &here).unwrap();
        assert_eq!(p.size, Size { w: 5000, h: 10000 });
        assert_eq!(
            p.pos,
            Pos {
                monitor: 0,
                x: 5000,
                y: 0
            }
        );
        // …is the right half of a 2560×1400 one.
        let there = [mon(0, 0, 2560, 1400, true)];
        let r = to_rect(Some(p.size), Some(p.pos), &there, None, HALF, (400, 300)).unwrap();
        assert_eq!(
            r,
            Rect {
                x: 1280,
                y: 0,
                w: 1280,
                h: 1400
            }
        );
    }

    #[test]
    fn the_monitor_is_found_by_overlap_and_missing_ones_fall_back() {
        let ms = [mon(0, 0, 1000, 1000, true), mon(1000, 0, 1000, 1000, false)];
        let mostly_right = Rect {
            x: 900,
            y: 100,
            w: 500,
            h: 500,
        };
        let p = to_placement(mostly_right, &ms).unwrap();
        assert_eq!(p.pos.monitor, 1);
        assert_eq!(p.pos.x, 0, "a corner before the screen clamps to its edge");
        // Saved on monitor 1; here there is only one: the primary.
        let one = [mon(0, 0, 800, 600, true)];
        let r = to_rect(Some(p.size), Some(p.pos), &one, None, HALF, (100, 100)).unwrap();
        assert_eq!((r.x, r.y), (0, 60));
        // Off every screen: the nearest one.
        let off = Rect {
            x: 5000,
            y: 0,
            w: 100,
            h: 100,
        };
        assert_eq!(to_placement(off, &ms).unwrap().pos.monitor, 1);
    }

    #[test]
    fn restored_windows_fit_on_their_screen() {
        let ms = [mon(0, 0, 1000, 800, true), mon(1000, 0, 1600, 900, false)];
        // At the far right edge and full width: pulled back fully on.
        let r = to_rect(
            Some(Size { w: 10000, h: 3000 }),
            Some(Pos {
                monitor: 0,
                x: 10000,
                y: 10000,
            }),
            &ms,
            None,
            HALF,
            (400, 300),
        )
        .unwrap();
        assert_eq!(
            r,
            Rect {
                x: 0,
                y: 500,
                w: 1000,
                h: 300
            }
        );
        // Tiny: the minimum. Unplaced: centred on the given monitor.
        let r = to_rect(
            Some(Size { w: 1, h: 1 }),
            None,
            &ms,
            Some(1),
            HALF,
            (400, 300),
        )
        .unwrap();
        assert_eq!(
            r,
            Rect {
                x: 1600,
                y: 300,
                w: 400,
                h: 300
            }
        );
        // A minimum larger than the screen: the whole screen.
        let small = [mon(0, 0, 300, 200, true)];
        let r = to_rect(None, None, &small, None, HALF, (400, 300)).unwrap();
        assert_eq!(
            r,
            Rect {
                x: 0,
                y: 0,
                w: 300,
                h: 200
            }
        );
        assert!(to_rect(None, None, &[], None, HALF, (1, 1)).is_none());
        let empty = [mon(0, 0, 0, 0, true)];
        assert!(to_rect(None, None, &empty, None, HALF, (1, 1)).is_none());
        let one_empty = [mon(0, 0, 0, 0, true), mon(10, 0, 100, 100, false)];
        assert_eq!(
            monitor_order(&one_empty),
            vec![1],
            "an empty monitor is skipped"
        );
    }

    #[test]
    fn save_then_restore_on_the_same_screen_is_stable() {
        let ms = [
            mon(-1920, 0, 1920, 1050, false),
            mon(0, 0, 2560, 1400, true),
        ];
        let window = Rect {
            x: -1500,
            y: 123,
            w: 777,
            h: 555,
        };
        let p = to_placement(window, &ms).unwrap();
        let r = to_rect(Some(p.size), Some(p.pos), &ms, None, HALF, (100, 100)).unwrap();
        assert!((r.x - window.x).abs() <= 1 && (r.y - window.y).abs() <= 1);
        assert!(r.w.abs_diff(window.w) <= 1 && r.h.abs_diff(window.h) <= 1);
    }
}
