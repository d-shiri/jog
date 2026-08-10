//! One clock for everything on screen that moves.
//!
//! Before this there were five: `tick % 10` for the dashboard glyph, `tick / 2`
//! for the hook-output spinner, `tick / 3` for the step bars, `tick / 5` and
//! `tick / 7` for the live strip. Nothing lined up, so a screen with three
//! animations on it read as three unrelated things twitching rather than one
//! interface that is alive. Every moving thing now derives from here.

use ratatui::style::Color;

/// Braille spinner, one full turn per second at the app's 100ms tick.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The animation clock, taken from the app's tick counter.
#[derive(Debug, Clone, Copy)]
pub struct Motion {
    tick: u64,
}

impl Motion {
    pub fn new(tick: u64) -> Self {
        Self { tick }
    }

    /// The spinner frame for right now.
    pub fn spinner(self) -> &'static str {
        SPINNER[(self.tick % SPINNER.len() as u64) as usize]
    }

    /// A slower spinner, for things that will take minutes rather than seconds.
    ///
    /// Same shape, half the speed: a run that has been going for eight minutes
    /// should not be twitching as urgently as a command that started just now.
    pub fn spinner_slow(self) -> &'static str {
        SPINNER[((self.tick / 2) % SPINNER.len() as u64) as usize]
    }

    /// 0.0 → 1.0 → 0.0 over `period` ticks.
    ///
    /// A triangle rather than a sine: the eye reads brightness roughly linearly,
    /// so a sine spends most of its time near the extremes and reads as a
    /// two-state blink instead of a breath.
    pub fn pulse(self, period: u64) -> f64 {
        let period = period.max(2);
        let half = period / 2;
        let t = self.tick % period;
        let up = if t < half { t } else { period - t };
        up as f64 / half as f64
    }

    /// A highlight travelling `0..span`, with a pause before it comes round
    /// again — a continuous loop reads as a strobe.
    pub fn sweep(self, span: usize, pause: usize) -> usize {
        if span == 0 {
            return 0;
        }
        (self.tick as usize / 2) % (span + pause)
    }

    /// A highlight bouncing back and forth across `span`, for waiting on
    /// something whose progress genuinely isn't known.
    pub fn bounce(self, span: usize) -> usize {
        if span <= 1 {
            return 0;
        }
        let raw = (self.tick as usize / 2) % (span * 2);
        if raw < span { raw } else { span * 2 - 1 - raw }
    }

    /// How much of an event that happened at `at` is left, from 1.0 down to 0.0
    /// over `over` ticks. Past that, and for anything in the future, 0.0.
    pub fn decay(self, at: u64, over: u64) -> f64 {
        let age = self.tick.saturating_sub(at);
        if at > self.tick || age >= over || over == 0 {
            return 0.0;
        }
        1.0 - (age as f64 / over as f64)
    }
}

/// Blend `t` of `b` into `a`.
///
/// Indexed colours cannot be blended — on a 256-colour terminal the palette has
/// already been folded down, and there is no arithmetic to do on a cell number.
/// Those snap at the halfway point instead, so the effect degrades to a flash
/// rather than disappearing.
pub fn mix(a: Color, b: Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let f = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
            Color::Rgb(f(ar, br), f(ag, bg), f(ab, bb))
        }
        _ if t >= 0.5 => b,
        _ => a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pulse_breathes_evenly_rather_than_blinking() {
        let at = |tick| Motion::new(tick).pulse(10);
        assert_eq!(at(0), 0.0);
        assert_eq!(at(5), 1.0);
        assert_eq!(at(10), 0.0, "and back again");
        // Evenly spaced: a sine would bunch the samples up at the ends, which is
        // what makes a "breath" read as an on/off blink.
        assert!((at(1) - 0.2).abs() < 1e-9);
        assert!((at(2) - 0.4).abs() < 1e-9);
        assert!((at(3) - 0.6).abs() < 1e-9);
    }

    #[test]
    fn a_flash_fades_to_nothing_and_stays_there() {
        let at = |tick| Motion::new(tick).decay(100, 10);
        assert_eq!(at(100), 1.0, "full strength the moment it happens");
        assert_eq!(at(105), 0.5);
        assert_eq!(at(110), 0.0);
        assert_eq!(at(400), 0.0, "long past, not wrapped round to bright again");
        // A tick before the event — a clock reset, a card loaded out of order.
        assert_eq!(at(99), 0.0);
    }

    #[test]
    fn a_sweep_pauses_instead_of_strobing() {
        let m = |tick| Motion::new(tick).sweep(4, 4);
        // Positions past the span are the gap: the highlight is off the bar.
        let cycle: Vec<usize> = (0..16).map(m).collect();
        assert!(cycle.iter().any(|&p| p >= 4), "no pause between passes");
        assert_eq!(m(0), m(16), "and it does come round again");
        assert_eq!(Motion::new(7).sweep(0, 4), 0, "no width, no crash");
    }

    #[test]
    fn a_bounce_turns_round_at_both_ends() {
        let path: Vec<usize> = (0..16).map(|t| Motion::new(t).bounce(4)).collect();
        assert_eq!(path, vec![0, 0, 1, 1, 2, 2, 3, 3, 3, 3, 2, 2, 1, 1, 0, 0]);
        assert_eq!(Motion::new(3).bounce(1), 0, "nowhere to go");
    }

    #[test]
    fn mixing_falls_back_to_a_snap_when_a_colour_cannot_be_blended() {
        let a = Color::Rgb(0, 0, 0);
        let b = Color::Rgb(100, 200, 40);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        assert_eq!(mix(a, b, 0.5), Color::Rgb(50, 100, 20));
        // 256-colour terminals: no arithmetic possible, so the effect becomes a
        // flash instead of vanishing.
        let idx = Color::Indexed(52);
        assert_eq!(mix(idx, b, 0.4), idx);
        assert_eq!(mix(idx, b, 0.6), b);
    }
}
