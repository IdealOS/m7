//! Spikes A and B: can wgpu output reach the screen through DRM/KMS?
//!
//! This is the copy path. wgpu renders the compositor's demo scene into an offscreen
//! texture. The pixels are read back and copied into one of two or three KMS dumb buffers,
//! which are page-flipped on vblank. It works with any Vulkan driver, lavapipe
//! included. The zero-copy dmabuf path for V3DV (Spike B) comes after this baseline.
//!
//! With `--direct` it tries Spike B instead: the GPU renders straight into the scanout
//! buffers, exported from the display as dma-bufs, with no readback or copy.
//!
//! With `--damage` it redraws only what changed each frame (the sliding window and the
//! readout), as the compositor does.
//!
//! `kms-probe [--frames N] [--windows N] [--hold SECS] [--buffers 2|3] [--direct] [--damage] [--card /dev/dri/cardN]`
//! prints a Markdown report. It needs DRM master: run it from a text console with no
//! display server, as root or a member of the `video` group.

use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
mod kms;

pub struct Options {
    pub frames: usize,
    pub windows: usize,
    pub hold: Duration,
    pub buffers: usize,
    /// `None` picks the first card with a connected display.
    pub card: Option<String>,
    /// Render straight into the scanout buffers (Spike B) instead of copying.
    pub direct: bool,
    /// Redraw only what changed each frame (region damage), not the whole screen.
    pub damage: bool,
}

impl Options {
    fn from_args() -> Result<Options, String> {
        let mut o = Options {
            frames: 300,
            windows: 20,
            hold: Duration::ZERO,
            buffers: 3,
            card: None,
            direct: false,
            damage: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--frames" => o.frames = value()?.parse().map_err(|e| format!("--frames: {e}"))?,
                "--windows" => o.windows = value()?.parse().map_err(|e| format!("--windows: {e}"))?,
                "--hold" => {
                    o.hold = Duration::from_secs(value()?.parse().map_err(|e| format!("--hold: {e}"))?)
                }
                "--card" => o.card = Some(value()?),
                "--direct" => o.direct = true,
                "--damage" => o.damage = true,
                "--buffers" => {
                    o.buffers = value()?.parse().map_err(|e| format!("--buffers: {e}"))?;
                    if !(2..=3).contains(&o.buffers) {
                        return Err("--buffers must be 2 or 3".into());
                    }
                }
                "-h" | "--help" => {
                    println!(
                        "kms-probe [--frames N] [--windows N] [--hold SECS] [--buffers 2|3] [--direct] [--damage] [--card PATH]"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(o)
    }
}

/// Per-stage timings for each frame.
#[derive(Default)]
pub struct Timings {
    pub render: Vec<Duration>,
    /// The parts of `render` (see compositor::headless::FrameTiming).
    pub encode: Vec<Duration>,
    pub gpu: Vec<Duration>,
    pub read: Vec<Duration>,
    pub copy: Vec<Duration>,
    pub flip_wait: Vec<Duration>,
    pub frame: Vec<Duration>,
    /// The share of the screen redrawn each frame (1.0 for a full redraw).
    pub damaged: Vec<f32>,
}

fn pct(v: &[Duration], p: f64) -> f64 {
    let mut v = v.to_vec();
    v.sort();
    v.get(((v.len().saturating_sub(1)) as f64 * p).round() as usize).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

impl Timings {
    pub fn report(&self, started: Instant, direct: bool) -> Vec<String> {
        let row = |name: &str, v: &[Duration]| {
            format!("| {name} | {:.2} ms | {:.2} ms |", pct(v, 0.5), pct(v, 0.99))
        };
        let elapsed = started.elapsed().as_secs_f64();
        let mut lines = vec!["| Stage | p50 | p99 |".into(), "| --- | --- | --- |".into()];
        if direct {
            lines.extend([
                row("Render into the scanout buffer", &self.render),
                row("  of which: encode (CPU)", &self.encode),
                row("  of which: GPU draws", &self.gpu),
            ]);
        } else {
            lines.extend([
                row("Render + GPU readback", &self.render),
                row("  of which: encode (CPU)", &self.encode),
                row("  of which: GPU draws and copies out", &self.gpu),
                row("  of which: read back (CPU)", &self.read),
                row("Copy into dumb buffer", &self.copy),
            ]);
        }
        lines.extend([
            row("Wait for page flip", &self.flip_wait),
            row("Whole frame", &self.frame),
            String::new(),
            format!(
                "Redrawn per frame: {:.1}% of the screen on average",
                100.0 * self.damaged.iter().sum::<f32>() / self.damaged.len().max(1) as f32
            ),
            format!(
                "{} frames in {:.2} s = {:.1} fps",
                self.frame.len(),
                elapsed,
                self.frame.len() as f64 / elapsed
            ),
        ]);
        lines
    }
}

/// Animate the demo: slide the topmost window and update the HUD text, so every frame
/// changes and text is reshaped, as it would be in real use.
pub fn animate(scene: &mut compositor::scene::Scene, frame: usize, hud: &str) {
    use protocol::SceneOp;
    let top = scene.top_level();
    // The desktop is first and the top bar last; the window just under the bar is the
    // frontmost. With no windows there's nothing to slide: moving the desktop itself
    // would make it stop covering the screen and measure a cost the desktop never has.
    if top.len() >= 3 {
        let id = top[top.len() - 2];
        let x = 300.0 + 200.0 * (frame as f32 / 30.0).sin();
        let props = serde_json::json!({ "x": x }).as_object().cloned().unwrap_or_default();
        scene.apply(SceneOp::Set { id, props });
    }
    scene.apply(compositor::demo::hud(hud));
}

#[cfg(target_os = "linux")]
fn run(o: &Options) -> Result<Vec<String>, String> {
    kms::run(o)
}

#[cfg(not(target_os = "linux"))]
fn run(_o: &Options) -> Result<Vec<String>, String> {
    Err("kms-probe drives DRM/KMS and only runs on Linux".into())
}

fn main() {
    let result = Options::from_args().and_then(|o| run(&o));
    match result {
        Ok(lines) => println!("{}", lines.join("\n")),
        Err(e) => {
            eprintln!("kms-probe: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animate_moves_frontmost_window() {
        let mut scene = compositor::scene::Scene::new();
        scene.apply_all(compositor::demo::build(1280.0, 800.0, 3));
        let top = scene.top_level().to_vec();
        let front = top[top.len() - 2];
        let before = scene.local_rect(front).unwrap();
        animate(&mut scene, 15, "frame 15");
        let after = scene.local_rect(front).unwrap();
        assert_ne!(before.x, after.x);
        assert_eq!(before.y, after.y);
    }

    #[test]
    fn percentiles() {
        let v: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        assert_eq!(pct(&v, 0.5).round(), 51.0);
        assert_eq!(pct(&v, 0.99), 99.0);
        assert_eq!(pct(&[], 0.5), 0.0);
    }
}
