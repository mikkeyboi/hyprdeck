//! Lighting-effect previews for the keyboard picture. Pure colour maths: given an animation, a
//! time and a key position, return the colour that key shows. The plugin names the effect; the
//! host draws an approximation of it, never a readback of the real LEDs.

use std::f64::consts::TAU;

use crate::protocol::{AnimationSpeed, KeyAnimation, KeyAnimationKind};

pub(crate) type Rgb = (f64, f64, f64);

/// How long a key press keeps lighting the picture, in seconds.
pub(crate) const PRESS_LIFE: f64 = 1.6;
/// Unlit keys in effects that leave most of the board dark.
const DARK: Rgb = (0.07, 0.09, 0.11);

/// A recent key press: where it happened (key units) and how long ago (seconds).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Press {
    pub x: f64,
    pub y: f64,
    pub age: f64,
}

/// Where a key sits on the board, in key units.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Spot {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub(crate) fn parse(value: &str) -> Option<Rgb> {
    let channel = |at: usize| {
        u8::from_str_radix(value.get(at..at + 2)?, 16)
            .ok()
            .map(|byte| f64::from(byte) / 255.0)
    };
    (value.len() == 7 && value.starts_with('#'))
        .then(|| Some((channel(1)?, channel(3)?, channel(5)?)))
        .flatten()
}

/// Whether the effect needs live key presses to show anything.
pub(crate) fn reactive(kind: KeyAnimationKind) -> bool {
    matches!(kind, KeyAnimationKind::Reactive | KeyAnimationKind::Ripple)
}

/// Whether the picture has to keep redrawing for this animation at `presses` recent presses.
pub(crate) fn moving(animation: &KeyAnimation, presses: usize) -> bool {
    match animation.kind {
        KeyAnimationKind::Unknown => false,
        kind if reactive(kind) => presses > 0,
        _ => true,
    }
}

fn rate(speed: AnimationSpeed) -> f64 {
    match speed {
        AnimationSpeed::Slow => 0.45,
        AnimationSpeed::Medium => 1.0,
        AnimationSpeed::Fast => 2.0,
    }
}

pub(crate) fn hsv(hue: f64, saturation: f64, value: f64) -> Rgb {
    let hue = hue.rem_euclid(1.0) * 6.0;
    let chroma = value * saturation;
    let x = chroma * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match hue as u8 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = value - chroma;
    (r + m, g + m, b + m)
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    (
        a.0 + (b.0 - a.0) * t,
        a.1 + (b.1 - a.1) * t,
        a.2 + (b.2 - a.2) * t,
    )
}

/// A looping colour ramp: the plugin's colours, or the rainbow when it gave none.
fn ramp(palette: &[Rgb], position: f64) -> Rgb {
    match palette {
        [] => hsv(position, 0.95, 1.0),
        [only] => *only,
        colors => {
            let at = position.rem_euclid(1.0) * colors.len() as f64;
            let index = at as usize % colors.len();
            mix(
                colors[index],
                colors[(index + 1) % colors.len()],
                smooth(at.fract()),
            )
        }
    }
}

fn smooth(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

/// Stable pseudo-random value in 0..1 for a pair of integers.
fn noise(a: i64, b: i64) -> f64 {
    let mut value = (a as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (b as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    value ^= value >> 29;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 32;
    (value % 10_000) as f64 / 10_000.0
}

/// The colour of the key at `spot`, `time` seconds into the animation.
pub(crate) fn color(animation: &KeyAnimation, time: f64, spot: Spot, presses: &[Press]) -> Rgb {
    let palette: Vec<Rgb> = animation
        .colors
        .iter()
        .filter_map(|value| parse(value))
        .collect();
    let t = time * rate(animation.speed);
    let (u, v) = (spot.x / spot.width.max(1.0), spot.y / spot.height.max(1.0));
    match animation.kind {
        KeyAnimationKind::Unknown => DARK,
        KeyAnimationKind::RainbowWave => hsv(u * 0.9 - t * 0.25, 0.95, 1.0),
        KeyAnimationKind::Wave => ramp(&palette, u * 0.8 - t * 0.25),
        KeyAnimationKind::Spiral => {
            let angle = (spot.y - spot.height / 2.0).atan2(spot.x - spot.width / 2.0) / TAU;
            let distance =
                (spot.x - spot.width / 2.0).hypot((spot.y - spot.height / 2.0) * 2.0) / spot.width;
            ramp(&palette, angle + distance * 0.6 - t * 0.3)
        }
        KeyAnimationKind::Shift => ramp(&palette, t * 0.12),
        KeyAnimationKind::Pulse => {
            let cycle = t * 0.4;
            let breath = (1.0 - (cycle.fract() * TAU).cos()) / 2.0;
            let tint = if palette.is_empty() {
                hsv(noise(cycle.floor() as i64, 7), 0.9, 1.0)
            } else {
                palette[cycle.floor() as usize % palette.len()]
            };
            mix(DARK, tint, breath)
        }
        KeyAnimationKind::Watercolor => {
            let hue = 0.5
                + 0.25 * (u * 5.0 + t * 0.5).sin()
                + 0.2 * (v * 4.0 - t * 0.35).cos()
                + 0.15 * ((u + v) * 3.0 + t * 0.2).sin();
            let soft = 0.55 + 0.2 * (u * 7.0 - v * 5.0 + t * 0.6).sin();
            if palette.is_empty() {
                hsv(hue, soft, 1.0)
            } else {
                mix(ramp(&palette, hue), (1.0, 1.0, 1.0), 0.25 - soft * 0.25)
            }
        }
        KeyAnimationKind::Rain => {
            // One drop per column, falling at its own pace and restarting with a new colour.
            let column = spot.x.floor() as i64;
            let fall = 2.2 + noise(column, 1) * 1.6;
            let span = spot.height + 4.0;
            let travelled = t * fall + noise(column, 2) * span;
            let head = travelled.rem_euclid(span) - 1.0;
            let behind = head - spot.y;
            let glow = if (0.0..3.0).contains(&behind) {
                (1.0 - behind / 3.0).powf(1.5)
            } else {
                0.0
            };
            let drop = (travelled / span).floor() as i64;
            let tint = if palette.is_empty() {
                hsv(noise(column, drop), 0.85, 1.0)
            } else {
                palette[(noise(column, drop) * palette.len() as f64) as usize % palette.len()]
            };
            mix(DARK, tint, glow)
        }
        KeyAnimationKind::Reactive => {
            let glow = presses
                .iter()
                .filter(|press| (press.x - spot.x).hypot(press.y - spot.y) < 0.6)
                .map(|press| (1.0 - press.age / 0.9).max(0.0))
                .fold(0.0, f64::max);
            mix(DARK, ramp(&palette, time * 0.1), glow)
        }
        KeyAnimationKind::Ripple => {
            let mut lit = DARK;
            for press in presses {
                let ring = press.age * 9.0;
                let distance = (press.x - spot.x).hypot((press.y - spot.y) * 1.4);
                let edge = (1.0 - (distance - ring).abs() / 1.1).max(0.0);
                let fade = (1.0 - press.age / PRESS_LIFE).max(0.0);
                let tint = ramp(&palette, distance * 0.05 + press.x * 0.07);
                lit = mix(lit, tint, edge * fade);
            }
            lit
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn animation(kind: KeyAnimationKind, colors: &[&str]) -> KeyAnimation {
        KeyAnimation {
            kind,
            speed: AnimationSpeed::Medium,
            colors: colors.iter().map(|color| (*color).to_owned()).collect(),
        }
    }

    fn spot(x: f64, y: f64) -> Spot {
        Spot {
            x,
            y,
            width: 16.0,
            height: 6.0,
        }
    }

    const ALL: [KeyAnimationKind; 9] = [
        KeyAnimationKind::RainbowWave,
        KeyAnimationKind::Wave,
        KeyAnimationKind::Spiral,
        KeyAnimationKind::Rain,
        KeyAnimationKind::Watercolor,
        KeyAnimationKind::Shift,
        KeyAnimationKind::Pulse,
        KeyAnimationKind::Reactive,
        KeyAnimationKind::Ripple,
    ];

    #[test]
    fn every_effect_stays_inside_the_colour_range_at_any_time() {
        let presses = [Press {
            x: 4.0,
            y: 2.0,
            age: 0.3,
        }];
        for kind in ALL {
            for palette in [&[][..], &["#FF0000"][..], &["#FF0000", "#0000FF"][..]] {
                let animation = animation(kind, palette);
                for step in 0..400 {
                    let time = f64::from(step) * 0.173;
                    for (x, y) in [(0.5, 0.5), (8.0, 3.0), (15.5, 5.5)] {
                        let (r, g, b) = color(&animation, time, spot(x, y), &presses);
                        for channel in [r, g, b] {
                            assert!(
                                (0.0..=1.0).contains(&channel),
                                "{kind:?} left the colour range: {channel}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn moving_effects_change_over_time_and_across_the_board() {
        let wave = animation(KeyAnimationKind::RainbowWave, &[]);
        assert_ne!(
            color(&wave, 0.0, spot(2.0, 2.0), &[]),
            color(&wave, 1.0, spot(2.0, 2.0), &[])
        );
        assert_ne!(
            color(&wave, 0.0, spot(2.0, 2.0), &[]),
            color(&wave, 0.0, spot(10.0, 2.0), &[])
        );
        // Colour shift lights the whole board alike.
        let shift = animation(KeyAnimationKind::Shift, &[]);
        assert_eq!(
            color(&shift, 3.0, spot(1.0, 1.0), &[]),
            color(&shift, 3.0, spot(14.0, 5.0), &[])
        );
    }

    #[test]
    fn speed_changes_the_pace_not_the_look() {
        let mut slow = animation(KeyAnimationKind::Wave, &["#FF0000", "#00FF00"]);
        slow.speed = AnimationSpeed::Slow;
        let mut fast = slow.clone();
        fast.speed = AnimationSpeed::Fast;
        assert_eq!(
            color(&slow, 0.0, spot(3.0, 1.0), &[]),
            color(&fast, 0.0, spot(3.0, 1.0), &[])
        );
        assert_ne!(
            color(&slow, 2.0, spot(3.0, 1.0), &[]),
            color(&fast, 2.0, spot(3.0, 1.0), &[])
        );
    }

    #[test]
    fn typing_effects_stay_dark_until_a_key_is_pressed() {
        for kind in [KeyAnimationKind::Reactive, KeyAnimationKind::Ripple] {
            let effect = animation(kind, &["#00FFAA"]);
            assert!(reactive(kind));
            assert!(!moving(&effect, 0));
            assert!(moving(&effect, 1));
            assert_eq!(color(&effect, 5.0, spot(4.0, 2.0), &[]), DARK);
        }
        let reactive = animation(KeyAnimationKind::Reactive, &["#00FFAA"]);
        let press = [Press {
            x: 4.0,
            y: 2.0,
            age: 0.0,
        }];
        assert_eq!(
            color(&reactive, 5.0, spot(4.0, 2.0), &press),
            parse("#00FFAA").unwrap()
        );
        assert_eq!(color(&reactive, 5.0, spot(9.0, 2.0), &press), DARK);
        // A ripple reaches a distant key later, not at once.
        let ripple = animation(KeyAnimationKind::Ripple, &["#00FFAA"]);
        let later = [Press {
            x: 4.0,
            y: 2.0,
            age: 0.55,
        }];
        assert_eq!(color(&ripple, 0.0, spot(9.0, 2.0), &press), DARK);
        assert_ne!(color(&ripple, 0.0, spot(9.0, 2.0), &later), DARK);
    }

    #[test]
    fn unknown_effects_never_animate() {
        let unknown = animation(KeyAnimationKind::Unknown, &[]);
        assert!(!moving(&unknown, 3));
    }

    #[test]
    fn colours_parse_strictly() {
        assert_eq!(parse("#FF0080"), Some((1.0, 0.0, 128.0 / 255.0)));
        assert_eq!(parse("FF0080"), None);
        assert_eq!(parse("#FF00"), None);
        assert_eq!(parse("#GG0000"), None);
    }
}
