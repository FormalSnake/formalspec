use std::time::{Duration, Instant};

use ratatui::style::Color;

pub const SLIDE: Duration = Duration::from_millis(160);
pub const SLIDE_COLS: i32 = 4;
pub const MARK: Duration = Duration::from_millis(800);
pub const FRAME: Duration = Duration::from_millis(16);
const SPIN_STEP_MS: u128 = 80;
const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn progress(start: Instant, now: Instant, dur: Duration) -> f32 {
    let el = now.saturating_duration_since(start).as_secs_f32();
    (el / dur.as_secs_f32()).clamp(0.0, 1.0)
}

pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

pub fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Color::Rgb(m(ar, br), m(ag, bg), m(ab, bb))
        }
        _ => b,
    }
}

pub fn spinner(epoch: Instant, now: Instant) -> &'static str {
    let ms = now.saturating_duration_since(epoch).as_millis();
    SPIN[(ms / SPIN_STEP_MS) as usize % SPIN.len()]
}

pub fn until_next_spin(epoch: Instant, now: Instant) -> Duration {
    let ms = now.saturating_duration_since(epoch).as_millis();
    Duration::from_millis((SPIN_STEP_MS - ms % SPIN_STEP_MS) as u64)
}
