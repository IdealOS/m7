//! Frame timing for the on-screen readout and the M1 performance check.

use std::time::Duration;

#[derive(Debug, Default)]
pub struct FrameTimer {
    frames: u32,
    total: Duration,
    worst: Duration,
    quads: usize,
    texts: usize,
}

impl FrameTimer {
    pub fn record(&mut self, cpu: Duration, quads: usize, texts: usize) {
        self.frames += 1;
        self.total += cpu;
        self.worst = self.worst.max(cpu);
        self.quads = quads;
        self.texts = texts;
    }

    pub fn reset_window(&mut self) {
        let (quads, texts) = (self.quads, self.texts);
        *self = FrameTimer { quads, texts, ..Default::default() };
    }

    /// One line for the top bar, e.g. `60 fps · avg 1.8 ms · max 3.1 ms · 412 quads`.
    pub fn summary(&self) -> String {
        if self.frames == 0 {
            return format!("idle · {} quads · {} texts", self.quads, self.texts);
        }
        let avg = self.total.as_secs_f64() * 1000.0 / self.frames as f64;
        format!(
            "{} fps · avg {:.1} ms · max {:.1} ms · {} quads",
            self.frames,
            avg,
            self.worst.as_secs_f64() * 1000.0,
            self.quads
        )
    }
}
