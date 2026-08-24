#![forbid(unsafe_code)]

//! Premium delight layer (OMP-ADOPT / bd-cv653.9.9).
//!
//! Provides working-message shimmer sweeps, magic-keyword gradient styling,
//! celebration fireworks particle systems, Unicode sparklines, and terminal title management.

use serde::{Deserialize, Serialize};

/// Shimmer wave type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShimmerMode {
    Cosine,
    Kitt,
}

/// Compute shimmer brightness (0.0 .. 1.0) for a given character index and tick.
#[must_use]
pub fn compute_shimmer_intensity(char_idx: usize, tick: u64, mode: ShimmerMode) -> f32 {
    let speed = 0.25;
    let width = 8.0;
    let phase = (tick as f32) * speed;

    match mode {
        ShimmerMode::Cosine => {
            let dist = ((char_idx as f32) - (phase % 40.0)).abs();
            if dist < width {
                0.15 + 0.85 * (1.0 + (dist / width * std::f32::consts::PI).cos()) * 0.5
            } else {
                0.15
            }
        }
        ShimmerMode::Kitt => {
            let cycle = 50.0;
            let ping_pong = ((phase % cycle) - (cycle / 2.0)).abs() * 2.0;
            let dist = ((char_idx as f32) - ping_pong).abs();
            if dist < width {
                (1.0 - (dist / width)).max(0.15)
            } else {
                0.15
            }
        }
    }
}

/// Render a series of numbers into an ASCII/Unicode sparkline string.
#[must_use]
pub fn render_sparkline(values: &[f64]) -> String {
    if values.is_empty() {
        return String::new();
    }

    const BARS: [char; 8] = [' ', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    let first = values
        .iter()
        .copied()
        .find(f64::is_finite)
        .unwrap_or(0.0);
    let mut min = first;
    let mut max = first;
    for &v in values {
        if v.is_finite() {
            if v < min {
                min = v;
            }
            if v > max {
                max = v;
            }
        }
    }

    let range = max - min;
    let mut out = String::with_capacity(values.len());

    for &v in values {
        let normalized = if range > 0.0 && v.is_finite() {
            (((v - min) / range * 7.0).round() as usize).min(7)
        } else {
            0
        };
        let bar_char = BARS.get(normalized.min(7)).copied().unwrap_or(' ');
        out.push(bar_char);
    }

    out
}

/// Single particle in the celebration fireworks system.
#[derive(Debug, Clone)]
pub struct Particle {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub life: u32,
    pub max_life: u32,
    pub glyph: char,
}

/// Fireworks animation state.
#[derive(Debug, Clone)]
pub struct FireworksState {
    pub particles: Vec<Particle>,
    pub frame_count: u32,
    pub is_active: bool,
}

impl Default for FireworksState {
    fn default() -> Self {
        Self {
            particles: Vec::new(),
            frame_count: 0,
            is_active: false,
        }
    }
}

impl FireworksState {
    pub fn trigger_burst(&mut self, origin_x: f32, origin_y: f32, count: usize) {
        self.is_active = true;
        let glyphs = ['*', '✦', '✧', '•', '·', 'x'];

        for i in 0..count {
            let angle = (i as f32) / (count as f32) * 2.0 * std::f32::consts::PI;
            let speed = 1.2 + (i % 3) as f32 * 0.4;
            let glyph = glyphs.get(i % glyphs.len()).copied().unwrap_or('*');

            self.particles.push(Particle {
                x: origin_x,
                y: origin_y,
                vx: angle.cos() * speed,
                vy: angle.sin() * speed * 0.6, // Aspect ratio compression
                life: 30,
                max_life: 30,
                glyph,
            });
        }
    }

    pub fn tick(&mut self) {
        if !self.is_active {
            return;
        }

        self.frame_count += 1;
        let gravity = 0.08;

        for p in &mut self.particles {
            p.x += p.vx;
            p.y += p.vy;
            p.vy += gravity;
            p.life = p.life.saturating_sub(1);
        }

        self.particles.retain(|p| p.life > 0);
        if self.particles.is_empty() {
            self.is_active = false;
        }
    }
}

/// Format OSC 0 / OSC 2 terminal window title escape sequence.
#[must_use]
pub fn format_terminal_title(title: &str) -> String {
    format!("\x1b]0;{title}\x07")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shimmer_intensity_bounds() {
        for tick in 0..20 {
            for idx in 0..30 {
                let intensity = compute_shimmer_intensity(idx, tick, ShimmerMode::Cosine);
                assert!((0.0..=1.0).contains(&intensity));
            }
        }
    }

    #[test]
    fn test_sparkline_generation() {
        let data = vec![1.0, 3.0, 5.0, 7.0, 9.0, 5.0, 2.0];
        let sparkline = render_sparkline(&data);
        assert_eq!(sparkline.chars().count(), 7);
        assert!(sparkline.starts_with(' '));
        assert!(sparkline.contains('█'));
    }

    #[test]
    fn test_fireworks_particle_lifecycle() {
        let mut fw = FireworksState::default();
        assert!(!fw.is_active);

        fw.trigger_burst(40.0, 12.0, 16);
        assert!(fw.is_active);
        assert_eq!(fw.particles.len(), 16);

        // Advance 35 frames until all particles fade
        for _ in 0..35 {
            fw.tick();
        }

        assert!(!fw.is_active);
        assert!(fw.particles.is_empty());
    }

    #[test]
    fn test_terminal_title_formatting() {
        let seq = format_terminal_title("Pi Agent - Session Alpha");
        assert!(seq.starts_with("\x1b]0;Pi Agent - Session Alpha\x07"));
    }
}
