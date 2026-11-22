//! The probe's frame loop on top of the compositor's KMS display
//! ([`compositor::backend::kms::display`]), timing each stage separately.

use std::time::{Duration, Instant};

use compositor::backend::kms::display::{Display, RENDER_FORMAT};
use compositor::headless::Offscreen;
use compositor::scene::{Rgba, Scene};

use crate::{Options, Timings, animate};

pub fn run(o: &Options) -> Result<Vec<String>, String> {
    let mut display = Display::open(&o.card, o.buffers)?;
    let (w, h) = display.size();

    let setup = Instant::now();
    let mut target = Offscreen::new(w, h, RENDER_FORMAT)?;
    let gpu_setup = setup.elapsed();

    let mut scene = Scene::new();
    scene.apply_all(compositor::demo::build(w as f32, h as f32, o.windows));

    let mut out = vec![
        format!("DRM driver: {}", display.driver()),
        format!("Output: {} at {}x{} @ {} Hz", display.output_name(), w, h, display.refresh_hz()),
        format!(
            "wgpu adapter: {} ({:?}, driver {} {}), set up in {:.0} ms",
            target.adapter.name,
            target.adapter.backend,
            target.adapter.driver,
            target.adapter.driver_info,
            gpu_setup.as_secs_f64() * 1000.0
        ),
        format!("Scene: {} windows, {} nodes; {} scanout buffers", o.windows, scene.len(), o.buffers),
        String::new(),
    ];

    let mut t = Timings::default();
    let started = Instant::now();
    let mut hud = String::from("starting");
    for frame in 0..o.frames.max(1) {
        let frame_start = Instant::now();
        let mut flip_wait = Duration::ZERO;
        // Double buffering: the free buffer is on screen until the pending flip lands.
        if display.buffer_count() < 3 {
            flip_wait += display.wait_for_flip()?;
        }
        animate(&mut scene, frame, &hud);

        // Render and read back into a staging Vec, then copy into the mapped dumb
        // buffer, so the two costs show up separately.
        let render_start = Instant::now();
        let mut rows: Vec<u8> = Vec::with_capacity((w * h * 4) as usize);
        target.render(&scene.draw_list(), 1.0, Rgba::BLACK, |row| rows.extend_from_slice(row))?;
        t.render.push(render_start.elapsed());

        let copy_start = Instant::now();
        display.copy_into_back(&rows)?;
        t.copy.push(copy_start.elapsed());

        flip_wait += display.flip()?;
        t.flip_wait.push(flip_wait);
        t.frame.push(frame_start.elapsed());
        if frame % 30 == 29 {
            let recent = &t.frame[t.frame.len() - 30..];
            let avg = recent.iter().sum::<Duration>().as_secs_f64() / 30.0;
            hud = format!("kms-probe  {:.1} ms/frame  {:.0} fps", avg * 1000.0, 1.0 / avg);
        }
    }
    display.wait_for_flip()?;
    out.extend(t.report(started));

    if !o.hold.is_zero() {
        // tools/spike-kms and tools/qemu-smoke.ts wait for this line before capturing.
        eprintln!("kms-probe: holding the last frame for {} s", o.hold.as_secs());
        std::thread::sleep(o.hold);
    }
    Ok(out)
}
