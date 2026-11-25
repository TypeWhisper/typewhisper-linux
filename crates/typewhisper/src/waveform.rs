//! Bar heights for the indicator's waveform, ported from the macOS app's
//! `AudioWaveformBarModel` so that both look alike. The GNOME Shell extension
//! has the same model in JavaScript.
//!
//! The bars follow the input level with a center-weighted shape and a slow
//! drift per bar, so they keep moving while the level holds. Attack and release
//! smoothing keeps them from jumping between the 30 level updates a second.
//! At the start, while the level is still near zero, one bar bounces from left
//! to right instead.

use std::f64::consts::PI;

/// Bars in the indicator, as in the macOS app's compact waveform.
pub const BAR_COUNT: usize = 5;
pub const BAR_WIDTH: f64 = 3.0;
pub const BAR_SPACING: f64 = 2.0;
pub const MAX_HEIGHT: f64 = 16.0;
/// The width all bars take.
pub const WIDTH: f64 = BAR_COUNT as f64 * BAR_WIDTH + (BAR_COUNT as f64 - 1.0) * BAR_SPACING;

const ATTACK: f64 = 0.04;
const RELEASE: f64 = 0.16;
const BOUNCE_INTERVAL: f64 = 0.06;
const BOUNCE_HEIGHT: f64 = 0.85;
/// How long the bounce may last while the level stays below `BOUNCE_LEVEL`.
pub const BOUNCE_SECONDS: f64 = 0.5;
pub const BOUNCE_LEVEL: f64 = 0.05;

pub struct BarModel {
    heights: [f64; BAR_COUNT],
    last_tick: Option<f64>,
}

impl BarModel {
    pub fn new() -> Self {
        Self {
            heights: [0.0; BAR_COUNT],
            last_tick: None,
        }
    }

    /// Moves the bars towards `level` (0..1) at `now` in seconds and returns
    /// their heights (0..1). `bounce` shows the start animation instead.
    /// Without `animate`, the bars take the level's shape at once and do not
    /// drift, for desktops with animations switched off.
    pub fn advance(
        &mut self,
        now: f64,
        level: f64,
        bounce: bool,
        animate: bool,
    ) -> [f64; BAR_COUNT] {
        let elapsed = self
            .last_tick
            .map_or(1.0 / 60.0, |last| (now - last).clamp(0.001, 0.1));
        self.last_tick = Some(now);
        let level = level.clamp(0.0, 1.0);
        let bouncing_bar = (now / BOUNCE_INTERVAL).floor() as usize % BAR_COUNT;
        for (index, height) in self.heights.iter_mut().enumerate() {
            let target = if bounce && animate {
                if index == bouncing_bar {
                    BOUNCE_HEIGHT
                } else {
                    0.0
                }
            } else if animate {
                level * shape(index) * drift(index, now)
            } else {
                level * shape(index)
            };
            if !animate {
                *height = target;
                continue;
            }
            let time_constant = if target > *height { ATTACK } else { RELEASE };
            *height += (target - *height) * (1.0 - (-elapsed / time_constant).exp());
        }
        self.heights
    }
}

/// Bars near the center reach higher than the outer ones.
fn shape(index: usize) -> f64 {
    let position = (index as f64 + 0.5) / BAR_COUNT as f64;
    0.55 + 0.45 * (PI * position).sin()
}

/// A slow oscillation with a different rate per bar, in 0.6..1.
fn drift(index: usize, now: f64) -> f64 {
    let rate = 2.2 + 0.65 * index as f64;
    let phase = index as f64 * 2.1;
    0.8 + 0.2 * (now * rate + phase).sin()
}

/// The pixel height of a bar: a dot of the bar's width at 0, `MAX_HEIGHT` at 1.
pub fn bar_height(height: f64) -> f64 {
    BAR_WIDTH + (MAX_HEIGHT - BAR_WIDTH) * height.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn center_bars_reach_highest() {
        assert!((shape(2) - 1.0).abs() < 1e-9);
        assert!((shape(0) - shape(4)).abs() < 1e-9);
        assert!(shape(0) < shape(1) && shape(1) < shape(2));
    }

    #[test]
    fn bars_rise_faster_than_they_fall() {
        let mut model = BarModel::new();
        let risen = model.advance(0.0, 1.0, false, true);
        model.advance(0.02, 1.0, false, true);
        let mut falling = BarModel::new();
        falling.heights = [1.0; BAR_COUNT];
        falling.advance(0.0, 0.0, false, true);
        let fallen = falling.advance(0.02, 0.0, false, true);
        // After the same 20 ms, rising covered more of the way than falling.
        assert!(risen[2] > 0.0);
        assert!(model.heights[2] > 1.0 - fallen[2]);
    }

    #[test]
    fn bounce_moves_from_left_to_right() {
        let mut model = BarModel::new();
        let mut step = 0;
        for interval in 0..10 {
            // Late in each 60 ms interval, its bar is the highest.
            let check = interval as f64 * BOUNCE_INTERVAL + 0.05;
            let mut heights = [0.0; BAR_COUNT];
            while step as f64 * 0.005 <= check {
                heights = model.advance(step as f64 * 0.005, 0.0, true, true);
                step += 1;
            }
            let highest = (0..BAR_COUNT)
                .max_by(|a, b| heights[*a].total_cmp(&heights[*b]))
                .unwrap();
            assert_eq!(highest, interval % BAR_COUNT);
        }
    }

    #[test]
    fn without_animations_the_bars_follow_the_level_at_once() {
        let mut model = BarModel::new();
        let heights = model.advance(0.0, 0.5, true, false);
        assert!((heights[2] - 0.5).abs() < 1e-9);
        assert!((heights[0] - 0.5 * shape(0)).abs() < 1e-9);
    }

    #[test]
    fn bar_heights_stay_between_a_dot_and_the_maximum() {
        assert_eq!(bar_height(0.0), BAR_WIDTH);
        assert_eq!(bar_height(1.0), MAX_HEIGHT);
        assert_eq!(bar_height(2.0), MAX_HEIGHT);
        assert_eq!(WIDTH, 23.0);
    }
}
