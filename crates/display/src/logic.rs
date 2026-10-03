//! Pure display math: mode parsing/grouping, Hyprland's scale validation and
//! the transform-aware arrangement geometry used by the layout editor.

/// One entry of `availableModes` (`3840x2160@119.88Hz`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mode {
    pub width: i64,
    pub height: i64,
    pub refresh: f64,
}

impl Mode {
    /// Parse `WxH@RHz`, `WxH@R` or `WxH` (refresh 0 = unspecified).
    pub fn parse(s: &str) -> Option<Mode> {
        let s = s.trim();
        let (res, rate) = match s.split_once('@') {
            Some((res, rate)) => (res, Some(rate.trim_end_matches("Hz"))),
            None => (s, None),
        };
        let (w, h) = res.split_once('x')?;
        let mode = Mode {
            width: w.trim().parse().ok()?,
            height: h.trim().parse().ok()?,
            refresh: match rate {
                Some(r) => r.trim().parse().ok()?,
                None => 0.0,
            },
        };
        (mode.width > 0 && mode.height > 0 && mode.refresh >= 0.0).then_some(mode)
    }

    /// Text for a monitor rule; same format as `availableModes`.
    pub fn to_rule(self) -> String {
        format!("{}x{}@{:.2}Hz", self.width, self.height, self.refresh)
    }
}

/// Refresh rates are compared at the 0.01 Hz precision hyprctl prints.
fn rate_key(r: f64) -> i64 {
    (r * 100.0).round() as i64
}

pub fn same_rate(a: f64, b: f64) -> bool {
    rate_key(a) == rate_key(b)
}

/// Unique resolutions, largest first.
pub fn resolutions(modes: &[Mode]) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = Vec::new();
    for m in modes {
        if !out.contains(&(m.width, m.height)) {
            out.push((m.width, m.height));
        }
    }
    out.sort_by(|a, b| (b.0 * b.1).cmp(&(a.0 * a.1)).then(b.0.cmp(&a.0)));
    out
}

/// Unique refresh rates offered for a resolution, fastest first.
pub fn rates(modes: &[Mode], width: i64, height: i64) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    for m in modes
        .iter()
        .filter(|m| m.width == width && m.height == height)
    {
        if !out.iter().any(|&r| same_rate(r, m.refresh)) {
            out.push(m.refresh);
        }
    }
    out.sort_by(|a, b| b.total_cmp(a));
    out
}

/// Index of the rate closest to `target`.
pub fn closest_rate(rates: &[f64], target: f64) -> Option<usize> {
    (0..rates.len()).min_by(|&a, &b| {
        (rates[a] - target)
            .abs()
            .total_cmp(&(rates[b] - target).abs())
    })
}

/// How Hyprland treats a configured scale for a given mode (see
/// `CMonitor::applyMonitorRule`: the rule's scale is a `float`; when
/// `pixels / scale` is fractional it searches `k/120` steps for a clean divisor).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScaleCheck {
    /// Used exactly as written.
    Exact,
    /// Silently replaced by the nearest 1/120 step, which divides cleanly.
    Rounded(f64),
    /// Rejected with an on-screen warning; Hyprland substitutes this value.
    Adjusted(f64),
    /// No clean divisor within ±0.75 of it; Hyprland falls back to its auto scale.
    Invalid,
}

fn clean(width: i64, height: i64, scale: f64) -> bool {
    let (lw, lh) = (width as f64 / scale, height as f64 / scale);
    lw == lw.round() && lh == lh.round()
}

pub fn check_scale(width: i64, height: i64, scale: f64) -> ScaleCheck {
    let configured = f64::from(scale as f32);
    if clean(width, height, configured) {
        return ScaleCheck::Exact;
    }
    let search = f64::from((configured * 120.0).round() as f32);
    let zero = search / 120.0;
    if clean(width, height, zero) {
        return ScaleCheck::Rounded(zero);
    }
    for i in 1..90 {
        let up = (search + i as f64) / 120.0;
        if clean(width, height, up) {
            return ScaleCheck::Adjusted(up);
        }
        let down = (search - i as f64) / 120.0;
        if down > 0.0 && clean(width, height, down) {
            return ScaleCheck::Adjusted(down);
        }
    }
    ScaleCheck::Invalid
}

/// The value to write so Hyprland applies the scale without correcting it.
pub fn snap_scale(width: i64, height: i64, scale: f64) -> Option<f64> {
    match check_scale(width, height, scale) {
        ScaleCheck::Exact => Some(scale),
        ScaleCheck::Rounded(s) | ScaleCheck::Adjusted(s) => Some(s),
        ScaleCheck::Invalid => None,
    }
}

/// All clean `k/120` scales in `[min, max]`.
pub fn valid_scales(width: i64, height: i64, min: f64, max: f64) -> Vec<f64> {
    let lo = (min * 120.0).ceil() as i64;
    let hi = (max * 120.0).floor() as i64;
    (lo.max(1)..=hi)
        .map(|k| k as f64 / 120.0)
        .filter(|&s| clean(width, height, s))
        .collect()
}

/// Clean scales nearest to the usual presets (1, 1.25, 1⅓, 1.5, …), skipping
/// any that would leave a logical height under 600 px.
pub fn suggested_scales(width: i64, height: i64) -> Vec<f64> {
    const TARGETS: [f64; 9] = [1.0, 1.25, 4.0 / 3.0, 1.5, 5.0 / 3.0, 1.75, 2.0, 2.5, 3.0];
    let valid = valid_scales(width, height, 0.5, 4.0);
    let mut out: Vec<f64> = Vec::new();
    for t in TARGETS {
        let Some(&best) = valid
            .iter()
            .min_by(|a, b| (*a - t).abs().total_cmp(&(*b - t).abs()))
        else {
            continue;
        };
        if (best - t).abs() <= 0.05
            && (height.min(width) as f64 / best) >= 600.0
            && !out.contains(&best)
        {
            out.push(best);
        }
    }
    out
}

/// Human scale text: up to 4 decimals without trailing zeros.
pub fn fmt_scale(s: f64) -> String {
    let t = format!("{s:.4}");
    let t = t.trim_end_matches('0');
    t.strip_suffix('.').unwrap_or(t).to_owned()
}

pub const TRANSFORMS: [&str; 8] = [
    "Normal",
    "Rotated 90°",
    "Rotated 180°",
    "Rotated 270°",
    "Flipped",
    "Flipped, rotated 90°",
    "Flipped, rotated 180°",
    "Flipped, rotated 270°",
];

/// Layout-space size of an output: pixels / scale with width/height swapped
/// for 90°/270° transforms; rounded like Hyprland's `m_size`.
pub fn logical_size(width: i64, height: i64, scale: f64, transform: i64) -> (f64, f64) {
    let (w, h) = if transform % 2 == 1 {
        (height, width)
    } else {
        (width, height)
    };
    let scale = if scale > 0.0 { scale } else { 1.0 };
    ((w as f64 / scale).round(), (h as f64 / scale).round())
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    /// Interiors intersect (touching edges is fine).
    pub fn overlaps(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }

    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px <= self.right() && py >= self.y && py <= self.bottom()
    }
}

/// Smallest rectangle containing all `rects`.
pub fn bounds(rects: &[Rect]) -> Option<Rect> {
    let first = rects.first()?;
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.right(), first.bottom());
    for r in &rects[1..] {
        x0 = x0.min(r.x);
        y0 = y0.min(r.y);
        x1 = x1.max(r.right());
        y1 = y1.max(r.bottom());
    }
    Some(Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// Offset that moves `[lo, hi]` onto the nearest of `edges` within `threshold`.
fn snap_axis(lo: f64, hi: f64, edges: &[f64], threshold: f64) -> f64 {
    let mut best = 0.0;
    let mut best_dist = threshold;
    for &e in edges {
        for d in [e - lo, e - hi] {
            if d.abs() < best_dist {
                best_dist = d.abs();
                best = d;
            }
        }
    }
    best
}

/// Move `moving` to its proposed spot, snapping its edges to other outputs'
/// edges within `threshold` layout px. `None` when the result would overlap.
pub fn place(moving: Rect, others: &[Rect], threshold: f64) -> Option<Rect> {
    let xs: Vec<f64> = others.iter().flat_map(|o| [o.x, o.right()]).collect();
    let ys: Vec<f64> = others.iter().flat_map(|o| [o.y, o.bottom()]).collect();
    let mut r = moving;
    r.x = (r.x + snap_axis(r.x, r.right(), &xs, threshold)).round();
    r.y = (r.y + snap_axis(r.y, r.bottom(), &ys, threshold)).round();
    (!others.iter().any(|o| o.overlaps(&r))).then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AW: [&str; 6] = [
        "3840x2160@60.00Hz",
        "3840x2160@240.00Hz",
        "3840x2160@119.88Hz",
        "1920x1080@60.00Hz",
        "2560x1440@120.00Hz",
        "640x480@59.94Hz",
    ];

    fn modes() -> Vec<Mode> {
        AW.iter().filter_map(|s| Mode::parse(s)).collect()
    }

    #[test]
    fn parses_modes() {
        assert_eq!(
            Mode::parse("3840x2160@119.88Hz"),
            Some(Mode {
                width: 3840,
                height: 2160,
                refresh: 119.88
            })
        );
        assert_eq!(
            Mode::parse("1920x1080@60"),
            Some(Mode {
                width: 1920,
                height: 1080,
                refresh: 60.0
            })
        );
        assert_eq!(Mode::parse("1920x1080").map(|m| m.refresh), Some(0.0));
        assert_eq!(Mode::parse("preferred"), None);
        assert_eq!(Mode::parse("0x1080@60"), None);
        assert_eq!(
            Mode::parse("3840x2160@119.88Hz").unwrap().to_rule(),
            "3840x2160@119.88Hz"
        );
    }

    #[test]
    fn groups_resolutions_and_rates() {
        let m = modes();
        assert_eq!(
            resolutions(&m),
            vec![(3840, 2160), (2560, 1440), (1920, 1080), (640, 480)]
        );
        assert_eq!(rates(&m, 3840, 2160), vec![240.0, 119.88, 60.0]);
        assert_eq!(rates(&m, 800, 600), Vec::<f64>::new());
        let r = rates(&m, 3840, 2160);
        assert_eq!(closest_rate(&r, 120.0), Some(1));
        assert_eq!(closest_rate(&[], 120.0), None);
        assert!(same_rate(119.88, 119.880_001));
    }

    #[test]
    fn scale_validation_matches_hyprland() {
        assert_eq!(check_scale(3840, 2160, 1.25), ScaleCheck::Exact);
        assert_eq!(check_scale(3840, 2160, 1.5), ScaleCheck::Exact);
        // 1.33 → 160/120, a clean divisor (2880x1620), accepted silently.
        assert_eq!(
            check_scale(3840, 2160, 1.33),
            ScaleCheck::Rounded(160.0 / 120.0)
        );
        // 1.3 → 156/120 does not divide; Hyprland searches upward first.
        match check_scale(3840, 2160, 1.3) {
            ScaleCheck::Adjusted(s) => {
                assert!(clean(3840, 2160, s) && (s - 1.3).abs() < 0.05, "{s}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(check_scale(1366, 768, 1.0), ScaleCheck::Exact);
        assert_eq!(snap_scale(3840, 2160, 1.25), Some(1.25));
    }

    #[test]
    fn valid_and_suggested_scales() {
        let v = valid_scales(3840, 2160, 1.0, 2.0);
        assert!(v.contains(&1.0) && v.contains(&1.25) && v.contains(&1.5) && v.contains(&2.0));
        assert!(!v.contains(&1.3));
        assert!(v.iter().all(|&s| clean(3840, 2160, s)));
        let s = suggested_scales(3840, 2160);
        assert_eq!(s[..2], [1.0, 1.25]);
        assert!(s.contains(&1.5) && s.contains(&2.0) && s.contains(&3.0));
        // 1080p: 2x would give 540 logical lines, below the 600 floor.
        assert!(!suggested_scales(1920, 1080).contains(&2.0));
        assert_eq!(fmt_scale(1.25), "1.25");
        assert_eq!(fmt_scale(2.0), "2");
        assert_eq!(fmt_scale(160.0 / 120.0), "1.3333");
    }

    #[test]
    fn logical_size_respects_transform() {
        assert_eq!(logical_size(3840, 2160, 1.25, 0), (3072.0, 1728.0));
        assert_eq!(logical_size(3840, 2160, 1.25, 1), (1728.0, 3072.0));
        assert_eq!(logical_size(3840, 2160, 1.25, 7), (1728.0, 3072.0));
        assert_eq!(logical_size(3840, 2160, 1.25, 6), (3072.0, 1728.0));
        assert_eq!(logical_size(1920, 1080, 1.0, 2), (1920.0, 1080.0));
    }

    #[test]
    fn placement_snaps_and_rejects_overlap() {
        let a = Rect {
            x: 0.0,
            y: 0.0,
            w: 3072.0,
            h: 1728.0,
        };
        // Dropped slightly right of A's right edge and slightly below its top: snaps flush.
        let b = Rect {
            x: 3090.0,
            y: 12.0,
            w: 1920.0,
            h: 1080.0,
        };
        assert_eq!(
            place(b, &[a], 40.0),
            Some(Rect {
                x: 3072.0,
                y: 0.0,
                ..b
            })
        );
        // Bottom edges snap too.
        let c = Rect {
            x: 3072.0,
            y: 640.0,
            w: 1920.0,
            h: 1080.0,
        };
        assert_eq!(place(c, &[a], 40.0).map(|r| r.bottom()), Some(1728.0));
        // Overlapping drop is refused.
        let d = Rect {
            x: 2000.0,
            y: 100.0,
            w: 1920.0,
            h: 1080.0,
        };
        assert_eq!(place(d, &[a], 40.0), None);
        // Far away: unchanged (rounded).
        let e = Rect {
            x: 5000.4,
            y: -2000.0,
            w: 100.0,
            h: 100.0,
        };
        assert_eq!(place(e, &[a], 40.0), Some(Rect { x: 5000.0, ..e }));
        assert!(!a.overlaps(&Rect {
            x: 3072.0,
            y: 0.0,
            w: 10.0,
            h: 10.0
        }));
        assert_eq!(
            bounds(&[
                a,
                Rect {
                    x: -100.0,
                    y: 1728.0,
                    w: 50.0,
                    h: 50.0
                }
            ]),
            Some(Rect {
                x: -100.0,
                y: 0.0,
                w: 3172.0,
                h: 1778.0
            })
        );
    }
}
