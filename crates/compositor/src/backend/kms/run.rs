//! The KMS backend's frame loop.

use std::time::{Duration, Instant};

use super::display::{Display, RENDER_FORMAT};
use crate::backend::Options;
use crate::demo;
use crate::headless::Offscreen;
use crate::scene::{Rgba, Scene};
use crate::stats::FrameTimer;

/// Scanout buffers. Three let a frame render while the previous flip is pending.
const BUFFERS: usize = 3;

pub fn run(opts: Options) -> Result<(), Box<dyn std::error::Error>> {
    let mut display = Display::open(&opts.card, BUFFERS)?;
    let (w, h) = display.size();
    let mut target = Offscreen::new(w, h, RENDER_FORMAT)?;
    log::info!(
        "kms: {} on {} at {w}x{h} @ {} Hz; adapter {} ({:?})",
        display.driver(),
        display.output_name(),
        display.refresh_hz(),
        target.adapter.name,
        target.adapter.backend,
    );

    let background = Rgba::parse(demo::BACKGROUND).expect("valid color");
    let mut scene = Scene::new();
    scene.apply_all(demo::build(w as f32, h as f32, opts.windows));
    let mut timer = FrameTimer::default();
    let mut next_hud = Instant::now();
    let mut rows: Vec<u8> = Vec::with_capacity((w * h * 4) as usize);
    let mut first = true;

    loop {
        // Refresh the frame-time readout once a second; while nothing else changes
        // this is the only redraw.
        let now = Instant::now();
        if now >= next_hud {
            let summary = timer.summary();
            if opts.continuous {
                log::info!("{summary}");
            }
            scene.apply(demo::hud(&summary));
            timer.reset_window();
            next_hud = now + Duration::from_secs(1);
        }

        if scene.is_dirty() || opts.continuous {
            let started = Instant::now();
            rows.clear();
            let stats = target.render(&scene.draw_list(), 1.0, background, |r| rows.extend_from_slice(r))?;
            display.copy_into_back(&rows)?;
            // Render, readback and copy: the CPU-side cost of a frame on this path.
            timer.record(started.elapsed(), stats.quads, stats.texts);
            display.flip()?;
            scene.clear_dirty();
            if first {
                // tools/qemu-smoke.ts waits for this line before capturing the display.
                log::info!("kms: first frame on screen");
                first = false;
            }
        } else {
            // Nothing to draw: let the last flip land, then sleep until the readout
            // is due. Input (M1) will replace the sleep with poll() on the libinput fd.
            display.wait_for_flip()?;
            std::thread::sleep(next_hud.saturating_duration_since(Instant::now()));
        }
    }
}
