//! Screen edges and the pointer across devices, in each OS's own desktop
//! coordinates (Windows: physical pixels; macOS: points; both y-down).
//!
//! An *edge band* is every display edge on one side with no other display
//! beyond it, in order — the outer edge of a multi-monitor desk. Positions
//! cross between devices as a fraction of the band's length.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    fn horizontal(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
}

/// One display in desktop coordinates. `scale` is native units per
/// device-independent pixel (Windows: DPI / 96; macOS: 1, points already are).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub scale: f64,
}

impl Screen {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// The line just outside the display on `side`.
    fn edge(&self, side: Side) -> f64 {
        match side {
            Side::Left => self.x,
            Side::Right => self.x + self.w,
            Side::Top => self.y,
            Side::Bottom => self.y + self.h,
        }
    }

    fn span(&self, side: Side) -> (f64, f64) {
        if side.horizontal() { (self.y, self.y + self.h) } else { (self.x, self.x + self.w) }
    }

    /// Whether this display covers the strip just beyond `line` on `side`.
    fn covers_beyond(&self, side: Side, line: f64) -> bool {
        let probe = match side {
            Side::Left | Side::Top => line - 0.5,
            Side::Right | Side::Bottom => line + 0.5,
        };
        let (lo, hi) = if side.horizontal() { (self.x, self.x + self.w) } else { (self.y, self.y + self.h) };
        probe >= lo && probe < hi
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    /// The edge line (just outside the display).
    pub line: f64,
    pub start: f64,
    pub end: f64,
    pub scale: f64,
}

/// The outer edge on `side`, ordered along it.
pub fn band(screens: &[Screen], side: Side) -> Vec<Segment> {
    let mut out = Vec::new();
    for (i, s) in screens.iter().enumerate() {
        let line = s.edge(side);
        let mut free = vec![s.span(side)];
        for (j, t) in screens.iter().enumerate() {
            if i == j || !t.covers_beyond(side, line) {
                continue;
            }
            let (b0, b1) = t.span(side);
            free = free
                .into_iter()
                .flat_map(|(a0, a1)| {
                    let mut parts = Vec::new();
                    if b1 <= a0 || b0 >= a1 {
                        parts.push((a0, a1));
                    } else {
                        if b0 > a0 {
                            parts.push((a0, b0));
                        }
                        if b1 < a1 {
                            parts.push((b1, a1));
                        }
                    }
                    parts
                })
                .collect();
        }
        out.extend(free.into_iter().filter(|(a, b)| b - a >= 1.0).map(|(start, end)| Segment { line, start, end, scale: s.scale }));
    }
    out.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap().then(a.line.partial_cmp(&b.line).unwrap()));
    out
}

fn total(band: &[Segment]) -> f64 {
    band.iter().map(|s| s.end - s.start).sum()
}

/// Where along the band a point sitting at the edge is, as 0..1.
pub fn fraction_at(band: &[Segment], side: Side, x: f64, y: f64) -> Option<f64> {
    let (along, across) = if side.horizontal() { (y, x) } else { (x, y) };
    let total = total(band);
    let mut before = 0.0;
    for seg in band {
        // The last pixel row/column inside the display.
        let inside = match side {
            Side::Left | Side::Top => seg.line,
            Side::Right | Side::Bottom => seg.line - 1.0,
        };
        if along >= seg.start && along < seg.end && (across - inside).abs() <= 1.5 {
            return Some(((before + along - seg.start) / total).clamp(0.0, 1.0));
        }
        before += seg.end - seg.start;
    }
    None
}

/// The point just inside the band at `fraction`.
pub fn point_at(band: &[Segment], side: Side, fraction: f64) -> Option<(f64, f64)> {
    let total = total(band);
    if total <= 0.0 {
        return None;
    }
    let mut target = fraction.clamp(0.0, 1.0) * total;
    for seg in band {
        let len = seg.end - seg.start;
        if target <= len || std::ptr::eq(seg, band.last().unwrap()) {
            let along = (seg.start + target.min(len - 1.0)).max(seg.start);
            let inset = 2.0 * seg.scale;
            let across = match side {
                Side::Left | Side::Top => seg.line + inset,
                Side::Right | Side::Bottom => seg.line - inset,
            };
            return Some(if side.horizontal() { (across, along) } else { (along, across) });
        }
        target -= len;
    }
    None
}

/// Decides when a pointer pushing against the shared edge should switch.
pub struct EdgeDetector {
    pub side: Side,
    pub delay_ms: u64,
    since: Option<u64>,
    hits: u32,
    quiet_until: u64,
}

/// No switching for this long after control came back, so a pointer still
/// moving toward the edge doesn't bounce straight back.
pub const SWITCH_COOLDOWN_MS: u64 = 400;

impl EdgeDetector {
    pub fn new(side: Side, delay_ms: u64) -> Self {
        Self { side, delay_ms, since: None, hits: 0, quiet_until: 0 }
    }

    pub fn cooldown(&mut self, now_ms: u64) {
        self.quiet_until = now_ms + SWITCH_COOLDOWN_MS;
        self.since = None;
        self.hits = 0;
    }

    /// Feed each local pointer position. Returns the edge fraction when the
    /// pointer has pushed against the edge long enough (at least twice, so a
    /// move that just stops at the edge doesn't switch).
    pub fn on_position(&mut self, band: &[Segment], x: f64, y: f64, now_ms: u64, button_held: bool) -> Option<f64> {
        if button_held || now_ms < self.quiet_until {
            self.since = None;
            self.hits = 0;
            return None;
        }
        let Some(fraction) = fraction_at(band, self.side, x, y) else {
            self.since = None;
            self.hits = 0;
            return None;
        };
        let since = *self.since.get_or_insert(now_ms);
        self.hits += 1;
        if self.hits >= 2 && now_ms.saturating_sub(since) >= self.delay_ms {
            self.cooldown(now_ms);
            return Some(fraction);
        }
        None
    }
}

/// The pointer on the controlled computer, driven by deltas from the
/// controller and placed with absolute coordinates (so the target's pointer
/// acceleration is never applied twice).
pub struct VirtualCursor {
    pub x: f64,
    pub y: f64,
    screens: Vec<Screen>,
    /// The side facing the controller.
    return_side: Side,
    return_band: Vec<Segment>,
}

#[derive(Debug, PartialEq)]
pub enum CursorStep {
    MovedTo(f64, f64),
    /// Pushed back across the shared edge at this fraction.
    Return(f64),
}

impl VirtualCursor {
    pub fn new(screens: Vec<Screen>, return_side: Side, fraction: f64) -> Option<Self> {
        let return_band = band(&screens, return_side);
        let (x, y) = point_at(&return_band, return_side, fraction).or_else(|| {
            screens.first().map(|s| (s.x + s.w / 2.0, s.y + s.h / 2.0))
        })?;
        Some(Self { x, y, screens, return_side, return_band })
    }

    fn scale_here(&self) -> f64 {
        self.screens.iter().find(|s| s.contains(self.x, self.y)).map_or(1.0, |s| s.scale)
    }

    pub fn step(&mut self, dx: f64, dy: f64) -> CursorStep {
        let scale = self.scale_here();
        let (tx, ty) = (self.x + dx * scale, self.y + dy * scale);
        if self.screens.iter().any(|s| s.contains(tx, ty)) {
            self.x = tx;
            self.y = ty;
            return CursorStep::MovedTo(tx, ty);
        }
        // Outside every display: clamp into the nearest one.
        let nearest = self
            .screens
            .iter()
            .min_by(|a, b| distance(a, tx, ty).partial_cmp(&distance(b, tx, ty)).unwrap())
            .copied();
        let Some(s) = nearest else { return CursorStep::MovedTo(self.x, self.y) };
        let cx = tx.clamp(s.x, s.x + s.w - 1.0);
        let cy = ty.clamp(s.y, s.y + s.h - 1.0);
        let pushed_out = match self.return_side {
            Side::Left => tx < cx,
            Side::Right => tx > cx,
            Side::Top => ty < cy,
            Side::Bottom => ty > cy,
        };
        if pushed_out {
            if let Some(fraction) = fraction_at(&self.return_band, self.return_side, cx, cy) {
                self.x = cx;
                self.y = cy;
                return CursorStep::Return(fraction);
            }
        }
        self.x = cx;
        self.y = cy;
        CursorStep::MovedTo(cx, cy)
    }
}

fn distance(s: &Screen, x: f64, y: f64) -> f64 {
    let dx = (s.x - x).max(0.0).max(x - (s.x + s.w - 1.0));
    let dy = (s.y - y).max(0.0).max(y - (s.y + s.h - 1.0));
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(x: f64, y: f64, w: f64, h: f64) -> Screen {
        Screen { x, y, w, h, scale: 1.0 }
    }

    #[test]
    fn single_display_band_is_its_whole_edge() {
        let s = [screen(0.0, 0.0, 1920.0, 1080.0)];
        let b = band(&s, Side::Right);
        assert_eq!(b, vec![Segment { line: 1920.0, start: 0.0, end: 1080.0, scale: 1.0 }]);
        assert_eq!(fraction_at(&b, Side::Right, 1919.0, 540.0), Some(0.5));
        assert_eq!(fraction_at(&b, Side::Right, 1000.0, 540.0), None);
    }

    #[test]
    fn edge_between_two_local_monitors_is_not_a_switch_edge() {
        // [A 1920x1080][B 1280x1024], B to the right of A, top-aligned.
        let s = [screen(0.0, 0.0, 1920.0, 1080.0), screen(1920.0, 0.0, 1280.0, 1024.0)];
        let right = band(&s, Side::Right);
        // Only B's right edge, plus the strip of A's right edge below B.
        assert!(right.iter().any(|g| g.line == 3200.0 && g.start == 0.0 && g.end == 1024.0));
        assert!(right.iter().any(|g| g.line == 1920.0 && g.start == 1024.0 && g.end == 1080.0));
        assert_eq!(fraction_at(&right, Side::Right, 1919.0, 500.0), None, "between monitors");
        assert!(fraction_at(&right, Side::Right, 3199.0, 500.0).is_some());
        let left = band(&s, Side::Left);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].line, 0.0);
    }

    #[test]
    fn fractions_map_between_different_resolutions() {
        let pc = band(&[screen(0.0, 0.0, 2560.0, 1440.0)], Side::Right);
        let mac = band(&[screen(0.0, 0.0, 1512.0, 982.0)], Side::Left);
        let f = fraction_at(&pc, Side::Right, 2559.0, 360.0).unwrap();
        let (x, y) = point_at(&mac, Side::Left, f).unwrap();
        assert_eq!(x, 2.0);
        assert!((y - 982.0 * 0.25).abs() < 1.0);
    }

    #[test]
    fn edge_needs_a_second_push_and_respects_delay_and_buttons() {
        let b = band(&[screen(0.0, 0.0, 1920.0, 1080.0)], Side::Right);
        let mut d = EdgeDetector::new(Side::Right, 0);
        assert_eq!(d.on_position(&b, 1919.0, 100.0, 1000, false), None, "first touch");
        assert!(d.on_position(&b, 1919.0, 100.0, 1010, false).is_some(), "second push switches");
        // Cooldown: pushing again right away doesn't switch.
        assert_eq!(d.on_position(&b, 1919.0, 100.0, 1020, false), None);
        assert_eq!(d.on_position(&b, 1919.0, 100.0, 1030, false), None);
        // A held button never switches.
        let mut d = EdgeDetector::new(Side::Right, 0);
        for t in 0..5 {
            assert_eq!(d.on_position(&b, 1919.0, 100.0, 2000 + t, true), None);
        }
        // With a delay, the push has to last.
        let mut d = EdgeDetector::new(Side::Right, 150);
        assert_eq!(d.on_position(&b, 1919.0, 100.0, 3000, false), None);
        assert_eq!(d.on_position(&b, 1919.0, 100.0, 3100, false), None);
        assert!(d.on_position(&b, 1919.0, 100.0, 3160, false).is_some());
        // Leaving the edge resets.
        let mut d = EdgeDetector::new(Side::Right, 0);
        d.on_position(&b, 1919.0, 100.0, 4000, false);
        d.on_position(&b, 1800.0, 100.0, 4010, false);
        assert_eq!(d.on_position(&b, 1919.0, 100.0, 4020, false), None);
    }

    #[test]
    fn virtual_cursor_moves_scales_clamps_and_returns() {
        // A Windows controlled side at 150% scaling, entered from the left.
        let s = vec![Screen { x: 0.0, y: 0.0, w: 2880.0, h: 1800.0, scale: 1.5 }];
        let mut c = VirtualCursor::new(s, Side::Left, 0.5).unwrap();
        assert_eq!((c.x, c.y), (3.0, 900.0));
        assert_eq!(c.step(10.0, 0.0), CursorStep::MovedTo(18.0, 900.0));
        // Running into the right edge clamps (no return there).
        assert_eq!(c.step(10_000.0, 0.0), CursorStep::MovedTo(2879.0, 900.0));
        // Back across the left edge: return at the same height.
        assert_eq!(c.step(-10_000.0, 0.0), CursorStep::Return(0.5));
    }

    #[test]
    fn virtual_cursor_crosses_between_its_own_monitors() {
        let s = vec![screen(0.0, 0.0, 1000.0, 800.0), screen(1000.0, 0.0, 1000.0, 800.0)];
        let mut c = VirtualCursor::new(s, Side::Left, 0.5).unwrap();
        c.step(990.0, 0.0);
        assert_eq!(c.step(20.0, 0.0), CursorStep::MovedTo(1012.0, 400.0));
        // The edge between the two monitors is not a return edge.
        assert!(matches!(c.step(-1500.0, 0.0), CursorStep::Return(_)));
    }
}
