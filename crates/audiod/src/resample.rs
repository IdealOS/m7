//! Sample-rate conversion for stereo audio: a windowed-sinc filter.
//!
//! Tracks come at 44.1 kHz, 48 kHz, 96 kHz and so on, and the output runs at one
//! rate, so that sources can be mixed. Audio at the output's rate passes through
//! untouched. The filter keeps its history between calls, so consecutive tracks at
//! the same rate join without a seam: gapless playback is one continuous stream.
//!
//! Output frame `n` is input time `n * step` (`step` = input rate / output rate).
//! The filter looks [`HALF`] input frames ahead, so the last frames of a stream
//! come out only once [`Resampler::finish`] pads it with silence.

/// Taps on each side of the centre.
const HALF: usize = 16;
/// Kernel phases between two input frames; others are interpolated.
const PHASES: usize = 256;

pub struct Resampler {
    /// The rates, in and out; equal rates pass audio through.
    from: u64,
    to: u64,
    /// `PHASES + 1` kernels of `2 * HALF` taps (unused when passing through).
    table: Vec<f32>,
    /// Input frames not yet fully used, as left/right pairs. Input frame `k` is
    /// at `k + HALF - dropped`: it starts with `HALF` frames of silence.
    hist: Vec<f32>,
    dropped: u64,
    /// Input frames taken since the start, and output frames made.
    taken: u64,
    made: u64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Resampler {
        let table = if from == to { Vec::new() } else { kernel(from as f64 / to as f64) };
        Resampler {
            from: from as u64,
            to: to as u64,
            table,
            hist: vec![0.0; 2 * HALF],
            dropped: 0,
            taken: 0,
            made: 0,
        }
    }

    /// Whether this converts `from` Hz to `to` Hz.
    pub fn converts(&self, from: u32, to: u32) -> bool {
        self.from == from as u64 && self.to == to as u64
    }

    /// The output frame at which input frame `taken` (counted from the start)
    /// comes out: output frame `n` is input time `n * from / to`.
    pub fn output_frame(&self, taken: u64) -> u64 {
        (taken * self.to).div_ceil(self.from)
    }

    /// Input frames taken so far.
    pub fn taken(&self) -> u64 {
        self.taken
    }

    /// Convert `input` (interleaved stereo), adding to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.taken += (input.len() / 2) as u64;
        if self.table.is_empty() {
            out.extend_from_slice(input);
            self.made += (input.len() / 2) as u64;
            return;
        }
        self.hist.extend_from_slice(input);
        self.run(out, u64::MAX);
    }

    /// The rest of the stream: what the lookahead held back.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        if self.table.is_empty() {
            return;
        }
        // Silence after the end fills the last windows; make output up to the
        // last real input frame, and no further.
        let due = self.output_frame(self.taken);
        self.hist.extend(std::iter::repeat_n(0.0, 2 * (HALF + 1)));
        self.run(out, due);
    }

    /// Make output frames while the window has the input it needs, up to `limit`.
    fn run(&mut self, out: &mut Vec<f32>, limit: u64) {
        let taps = 2 * HALF;
        let frames = (self.hist.len() / 2) as u64 + self.dropped;
        while self.made < limit {
            let num = self.made * self.from;
            let i = num / self.to;
            // Taps cover input frames i - HALF + 1 ..= i + HALF.
            if i + 2 * HALF as u64 >= frames {
                break;
            }
            let p = (num % self.to) as f64 / self.to as f64 * PHASES as f64;
            let (k0, t) = (p as usize, (p - p.floor()) as f32);
            let (a, b) = (&self.table[k0 * taps..][..taps], &self.table[(k0 + 1) * taps..][..taps]);
            let start = (i + 1 - self.dropped) as usize;
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for j in 0..taps {
                let w = a[j] + (b[j] - a[j]) * t;
                l += self.hist[2 * (start + j)] * w;
                r += self.hist[2 * (start + j) + 1] * w;
            }
            out.push(l);
            out.push(r);
            self.made += 1;
        }
        // Drop frames no window reaches any more.
        let first_needed = (self.made * self.from / self.to + 1).min(frames);
        let drop = first_needed.saturating_sub(self.dropped) as usize;
        if drop > 0 {
            self.hist.drain(..2 * drop.min(self.hist.len() / 2));
            self.dropped += drop as u64;
        }
    }
}

/// The kernels: a Blackman-windowed sinc, cut off below the lower of the two
/// rates' Nyquist frequencies.
fn kernel(step: f64) -> Vec<f32> {
    let cutoff = if step > 1.0 { 0.97 / step } else { 0.97 };
    let taps = 2 * HALF;
    let mut table = Vec::with_capacity((PHASES + 1) * taps);
    for phase in 0..=PHASES {
        let frac = phase as f64 / PHASES as f64;
        let mut row: Vec<f64> = (0..taps)
            .map(|j| {
                // Distance from the output point to input frame i - HALF + 1 + j.
                let x = (j as f64 - (HALF as f64 - 1.0)) - frac;
                let sinc = if x.abs() < 1e-9 {
                    1.0
                } else {
                    (std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * cutoff * x)
                };
                let n = (x / HALF as f64 + 1.0) / 2.0; // 0..1 across the window
                let window = if (0.0..=1.0).contains(&n) {
                    0.42 - 0.5 * (2.0 * std::f64::consts::PI * n).cos()
                        + 0.08 * (4.0 * std::f64::consts::PI * n).cos()
                } else {
                    0.0
                };
                cutoff * sinc * window
            })
            .collect();
        // Unity gain at DC, whatever the phase.
        let sum: f64 = row.iter().sum();
        if sum.abs() > 1e-9 {
            row.iter_mut().for_each(|w| *w /= sum);
        }
        table.extend(row.iter().map(|&w| w as f32));
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let v = (2.0 * std::f64::consts::PI * hz * n as f64 / rate as f64).sin() as f32 * 0.5;
                [v, -v]
            })
            .collect()
    }

    #[test]
    fn the_same_rate_passes_through_untouched() {
        let mut r = Resampler::new(48_000, 48_000);
        let input = sine(48_000, 440.0, 1000);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        r.finish(&mut out);
        assert_eq!(out, input);
        assert_eq!(r.output_frame(1000), 1000);
    }

    #[test]
    fn makes_the_right_number_of_frames_in_any_size_of_piece() {
        for (from, to) in [(44_100, 48_000), (96_000, 48_000), (22_050, 44_100)] {
            let input = sine(from, 1000.0, from as usize); // one second
            let mut whole = Vec::new();
            let mut r = Resampler::new(from, to);
            r.process(&input, &mut whole);
            r.finish(&mut whole);
            assert_eq!(whole.len() / 2, to as usize, "{from} -> {to}");
            // Fed in odd-sized pieces, the same samples come out.
            let mut pieces = Vec::new();
            let mut r = Resampler::new(from, to);
            for chunk in input.chunks(2 * 333) {
                r.process(chunk, &mut pieces);
            }
            r.finish(&mut pieces);
            assert_eq!(pieces.len(), whole.len());
            assert!(pieces.iter().zip(&whole).all(|(a, b)| (a - b).abs() < 1e-6));
        }
    }

    #[test]
    fn a_tone_keeps_its_pitch_and_level() {
        // 1 kHz at 44.1 kHz, converted to 48 kHz, compared with 1 kHz made at 48 kHz.
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        r.process(&sine(44_100, 1000.0, 44_100), &mut out);
        r.finish(&mut out);
        let want = sine(48_000, 1000.0, 48_000);
        // Away from the edges, where the window sees silence.
        let worst = out[2000..90_000]
            .iter()
            .zip(&want[2000..90_000])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 0.002, "off by {worst}");
    }

    #[test]
    fn downsampling_removes_what_the_output_cannot_hold() {
        // 30 kHz can't exist at 48 kHz (its limit is 24 kHz): it must not fold back
        // down as an 18 kHz tone.
        let mut r = Resampler::new(96_000, 48_000);
        let mut out = Vec::new();
        r.process(&sine(96_000, 30_000.0, 96_000), &mut out);
        r.finish(&mut out);
        let peak = out[2000..90_000].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.01, "aliased at {peak}");
    }

    #[test]
    fn output_frames_line_up_with_input_frames() {
        let r = Resampler::new(44_100, 48_000);
        assert_eq!(r.output_frame(0), 0);
        assert_eq!(r.output_frame(44_100), 48_000);
        assert_eq!(r.output_frame(441), 480);
    }
}
