//! Speeding speech up or slowing it down without changing its pitch.
//!
//! WSOLA (waveform similarity overlap-add): the output is built from
//! windowed frames of the input, half-overlapped, taken `speed` times
//! further apart than they're laid down. Each frame is nudged a little so
//! its start matches the waveform the last frame was heading into, which
//! keeps voices from warbling.

/// Samples in one frame: about 21 ms at 24 kHz, enough to hold a pitch period.
const FRAME: usize = 512;
/// Output samples per frame: frames overlap by half.
const HOP: usize = FRAME / 2;
/// How far either way a frame may move to line up with the last one.
const REACH: usize = 120;
/// Every this many samples are compared when lining frames up; enough to
/// find the best fit at a fraction of the work.
const STRIDE: usize = 2;

/// One sound being stretched. Feed it the whole clip; pull output a block at a time.
pub struct Stretch {
    window: Vec<f32>,
    /// Where the next frame would be taken from at the current speed.
    nominal: f64,
    /// Where the last frame was taken from.
    previous: Option<usize>,
    /// The falling half of the last frame, still to be added to the next.
    tail: Vec<f32>,
    /// Output samples ready to hand out, and how many have been.
    ready: Vec<f32>,
    taken: usize,
}

impl Default for Stretch {
    fn default() -> Self {
        // A periodic Hann window: half-overlapped copies sum to exactly one,
        // so at normal speed the input comes out unchanged.
        let window = (0..FRAME)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FRAME as f32).cos())
            .collect();
        Stretch {
            window,
            nominal: 0.0,
            previous: None,
            tail: vec![0.0; HOP],
            ready: Vec::new(),
            taken: 0,
        }
    }
}

impl Stretch {
    /// Starts playing from sample `at` of the clip.
    pub fn at(at: usize) -> Self {
        Stretch {
            nominal: at as f64,
            ..Stretch::default()
        }
    }

    /// Where in `clip` the sound being played comes from: the middle of the
    /// frame now being handed out.
    pub fn position(&self) -> usize {
        match self.previous {
            Some(previous) => previous + self.taken.min(HOP),
            None => self.nominal as usize,
        }
    }

    /// The next output sample of `clip` played at `speed`, or `None` at its end.
    pub fn next(&mut self, clip: &[f32], speed: f32) -> Option<f32> {
        if self.taken >= self.ready.len() {
            if self.nominal as usize >= clip.len() {
                return None;
            }
            self.step(clip, speed);
        }
        let sample = self.ready[self.taken];
        self.taken += 1;
        Some(sample)
    }

    /// Lays down one more hop of output.
    fn step(&mut self, clip: &[f32], speed: f32) {
        let at = |i: isize| -> f32 {
            if i < 0 {
                0.0
            } else {
                clip.get(i as usize).copied().unwrap_or(0.0)
            }
        };
        let nominal = self.nominal as usize;
        let start = match self.previous {
            // At normal speed the natural continuation is exactly where the
            // next frame would be taken, so there's nothing to line up.
            Some(previous) if (speed - 1.0).abs() > f32::EPSILON => {
                let natural = (previous + HOP) as isize;
                let lowest = nominal.saturating_sub(REACH);
                let highest = nominal + REACH;
                let mut best = (f32::MIN, nominal);
                for candidate in lowest..=highest {
                    let score: f32 = (0..HOP)
                        .step_by(STRIDE)
                        .map(|i| at(natural + i as isize) * at((candidate + i) as isize))
                        .sum();
                    if score > best.0 {
                        best = (score, candidate);
                    }
                }
                best.1
            }
            _ => nominal,
        };
        self.ready.clear();
        self.taken = 0;
        for i in 0..HOP {
            self.ready
                .push(self.tail[i] + at((start + i) as isize) * self.window[i]);
            self.tail[i] = at((start + HOP + i) as isize) * self.window[HOP + i];
        }
        self.previous = Some(start);
        self.nominal += HOP as f64 * speed.max(0.1) as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(len: usize, period: f32) -> Vec<f32> {
        (0..len)
            .map(|i| (std::f32::consts::TAU * i as f32 / period).sin())
            .collect()
    }

    fn play(clip: &[f32], speed: f32) -> Vec<f32> {
        let mut stretch = Stretch::default();
        std::iter::from_fn(|| stretch.next(clip, speed)).collect()
    }

    /// Zero crossings going up, a measure of pitch.
    fn crossings(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count()
    }

    #[test]
    fn normal_speed_gives_back_the_input() {
        let clip = tone(24_000, 97.0);
        let out = play(&clip, 1.0);
        // The first half frame fades in; past it, the output is the input.
        for i in HOP..clip.len() - FRAME {
            assert!((out[i] - clip[i]).abs() < 1e-4, "sample {i}");
        }
    }

    #[test]
    fn double_speed_halves_the_length_and_keeps_the_pitch() {
        let clip = tone(48_000, 97.0);
        let out = play(&clip, 2.0);
        let expected = clip.len() / 2;
        assert!(out.len().abs_diff(expected) <= HOP, "{} samples", out.len());
        let (input_rate, output_rate) = (
            crossings(&clip) as f32 / clip.len() as f32,
            crossings(&out) as f32 / out.len() as f32,
        );
        assert!((output_rate / input_rate - 1.0).abs() < 0.05);
    }

    #[test]
    fn half_speed_doubles_the_length() {
        let clip = tone(24_000, 97.0);
        let out = play(&clip, 0.5);
        assert!(out.len().abs_diff(clip.len() * 2) <= HOP * 2);
    }

    #[test]
    fn the_position_follows_the_input_at_any_speed() {
        let clip = tone(48_000, 97.0);
        let mut stretch = Stretch::default();
        for _ in 0..12_000 {
            stretch.next(&clip, 2.0);
        }
        assert!(stretch.position().abs_diff(24_000) <= REACH + HOP);
    }

    #[test]
    fn playing_can_start_partway_in() {
        let clip = tone(48_000, 97.0);
        let mut stretch = Stretch::at(30_000);
        let out: Vec<f32> = std::iter::from_fn(|| stretch.next(&clip, 1.0)).collect();
        assert!(out.len().abs_diff(18_000) <= HOP);
        assert!(stretch.position() >= 47_000);
    }
}
