//! Screens, points, and the mapping between what a caller means and what the
//! agent is told.
//!
//! Everything here is deliberately on this side of the wire. Coordinate maths
//! spread across per-platform agents is how remote-input tools acquire their
//! signature bug — clicks landing at a consistent fraction of the intended
//! offset, which reads as bad aim rather than as a unit mismatch.

use serde::{Deserialize, Serialize};

/// A physical pixel in virtual-desktop coordinates. The origin is the
/// primary screen's top-left, so a monitor placed to its left has negative
/// `x`: this is signed for a reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// Physical pixel bounds in virtual-desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x
            && p.y >= self.y
            && (p.x - self.x) < self.w as i32
            && (p.y - self.y) < self.h as i32
    }

    /// Nearest point inside the rect. A zero-area rect clamps to its origin.
    pub fn clamp(&self, p: Point) -> Point {
        let max_x = self.x + self.w.saturating_sub(1) as i32;
        let max_y = self.y + self.h.saturating_sub(1) as i32;
        Point::new(p.x.clamp(self.x, max_x), p.y.clamp(self.y, max_y))
    }
}

/// Stable across reboots, driver updates and replugging. Never an index:
/// Windows reassigns GDI device indices on all three, so an agent keyed by
/// index eventually clicks confidently on the wrong monitor.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScreenId(pub String);

impl From<&str> for ScreenId {
    fn from(s: &str) -> Self {
        ScreenId(s.to_string())
    }
}

impl From<String> for ScreenId {
    fn from(s: String) -> Self {
        ScreenId(s)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    pub id: ScreenId,
    /// Physical pixels, virtual-desktop coordinates.
    pub bounds: Rect,
    /// 1.0 at 96 dpi; 1.5 at 150%. Carried so a caller can register an image
    /// captured by some other process against this screen — see
    /// `image_to_physical`.
    pub scale: f32,
    pub primary: bool,
    /// For logs and for a human choosing a monitor. Not an identity.
    #[serde(default)]
    pub label: String,
}

impl Screen {
    /// Map a pixel in an image *of this screen* to a physical point on it.
    ///
    /// The reason this exists: a screenshot taken by a process that is not
    /// per-monitor DPI aware comes back virtualized — a 2560×1440 monitor at
    /// 150% captures as 1707×960. Injection works in physical pixels, so a
    /// caller reasoning about that image and sending its pixel coordinates
    /// straight through lands every click at 0.667× the intended offset.
    /// Scaling by the actual image dimensions fixes it whatever the cause,
    /// virtualization or a plain resize before the image was handed on.
    pub fn image_to_physical(&self, img_w: u32, img_h: u32, ix: f64, iy: f64) -> Option<Point> {
        if img_w == 0 || img_h == 0 {
            return None;
        }
        if ix < 0.0 || iy < 0.0 || ix >= img_w as f64 || iy >= img_h as f64 {
            return None;
        }
        // An image pixel is an *area sample*, not a position: pixel `i` of a
        // 1707-wide capture covers physical pixels `[i·s, (i+1)·s)`. So the
        // mapping goes through pixel centres — `(i + 0.5)·s - 0.5` — and not
        // through `normalized_to_physical`, whose input is a position where
        // 1.0 means the far edge. Treating the two alike puts the bottom-right
        // of a downscaled capture a pixel and a half short of the corner.
        let map = |i: f64, img: u32, phys: u32| -> i32 {
            let scale = phys as f64 / img as f64;
            (((i + 0.5) * scale) - 0.5).round() as i32
        };
        let hi = |len: u32| len.saturating_sub(1) as i32;
        Some(Point::new(
            self.bounds.x + map(ix, img_w, self.bounds.w).clamp(0, hi(self.bounds.w)),
            self.bounds.y + map(iy, img_h, self.bounds.h).clamp(0, hi(self.bounds.h)),
        ))
    }

    /// `0.0..=1.0` within this screen. `1.0` is the last pixel, not one past
    /// the edge — the off-by-one that puts a click on the neighbouring
    /// monitor.
    pub fn normalized_to_physical(&self, fx: f64, fy: f64) -> Option<Point> {
        if !(0.0..=1.0).contains(&fx) || !(0.0..=1.0).contains(&fy) {
            return None;
        }
        let span = |n: f64, len: u32| (n * len.saturating_sub(1) as f64).round() as i32;
        Some(Point::new(
            self.bounds.x + span(fx, self.bounds.w),
            self.bounds.y + span(fy, self.bounds.h),
        ))
    }
}

/// The agent's screen layout, plus the token that says which layout this is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Screens {
    pub screens: Vec<Screen>,
    /// Increments whenever the display configuration changes. It detects a
    /// layout change between a caller's screenshot and its click, and it does
    /// not detect a *content* change — for that the capture side has to stamp
    /// its own images. Documented rather than implied, so nobody trusts it
    /// for more than it does.
    pub state: u64,
}

impl Screens {
    pub fn get(&self, id: &ScreenId) -> Option<&Screen> {
        self.screens.iter().find(|s| &s.id == id)
    }

    pub fn primary(&self) -> Option<&Screen> {
        self.screens
            .iter()
            .find(|s| s.primary)
            .or_else(|| self.screens.first())
    }

    /// The screen a physical point falls on, if any.
    pub fn hit(&self, p: Point) -> Option<&Screen> {
        self.screens.iter().find(|s| s.bounds.contains(p))
    }
}

/// Where a caller wants the pointer. Normalized is the default because a
/// caller naming raw pixels on a machine whose layout it has not read is
/// guessing; `Absolute` is for a caller that has read `screens`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "at", rename_all = "snake_case")]
pub enum Loc {
    Absolute { x: i32, y: i32 },
    Normalized { screen: ScreenId, x: f64, y: f64 },
}

impl Loc {
    pub fn absolute(x: i32, y: i32) -> Self {
        Loc::Absolute { x, y }
    }

    pub fn normalized(screen: impl Into<ScreenId>, x: f64, y: f64) -> Self {
        Loc::Normalized {
            screen: screen.into(),
            x,
            y,
        }
    }
}
