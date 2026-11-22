//! The DRM/KMS side: find a display, allocate dumb buffers, and page-flip.
//!
//! With two buffers the next frame cannot start until the pending flip completes,
//! because the only free buffer is still on screen. With three, the next frame renders
//! while the flip is pending, so work that nearly fills a vblank period no longer
//! costs a whole extra period.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::time::Instant;

use drm::Device as _;
use drm::buffer::{Buffer as _, DrmFourcc};
use drm::control::{
    Device as ControlDevice, Event, Mode, ModeTypeFlags, PageFlipFlags, connector, crtc,
    dumbbuffer::DumbBuffer, framebuffer,
};

use compositor::headless::Offscreen;
use compositor::scene::{Rgba, Scene};

use crate::{Options, Timings, animate};

struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl ControlDevice for Card {}

struct Output {
    connector: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    name: String,
}

fn find_output(card: &Card) -> Result<Output, String> {
    let res = card.resource_handles().map_err(|e| format!("resource handles: {e}"))?;
    for &handle in res.connectors() {
        let info = card.get_connector(handle, true).map_err(|e| format!("get connector: {e}"))?;
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            continue;
        }
        let mode = info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .copied()
            .unwrap_or(info.modes()[0]);
        let encoder = info.current_encoder().or_else(|| info.encoders().first().copied());
        let crtc = encoder
            .and_then(|e| card.get_encoder(e).ok())
            .and_then(|e| e.crtc().or_else(|| res.filter_crtcs(e.possible_crtcs()).first().copied()))
            .ok_or("no CRTC for the connected connector")?;
        let name = format!("{:?}-{}", info.interface(), info.interface_id());
        return Ok(Output { connector: handle, crtc, mode, name });
    }
    Err("no connected display".into())
}

struct Scanout {
    buffer: DumbBuffer,
    fb: framebuffer::Handle,
}

fn scanout(card: &Card, w: u32, h: u32) -> Result<Scanout, String> {
    let buffer = card
        .create_dumb_buffer((w, h), DrmFourcc::Xrgb8888, 32)
        .map_err(|e| format!("create dumb buffer: {e}"))?;
    let fb = card.add_framebuffer(&buffer, 24, 32).map_err(|e| format!("add framebuffer: {e}"))?;
    Ok(Scanout { buffer, fb })
}

fn wait_for_flip(card: &Card) -> Result<(), String> {
    loop {
        for event in card.receive_events().map_err(|e| format!("receive events: {e}"))? {
            if let Event::PageFlip(_) = event {
                return Ok(());
            }
        }
    }
}

pub fn run(o: &Options) -> Result<Vec<String>, String> {
    let file =
        OpenOptions::new().read(true).write(true).open(&o.card).map_err(|e| format!("{}: {e}", o.card))?;
    let card = Card(file);
    let driver = card.get_driver().map_err(|e| format!("get driver: {e}"))?;
    let output = find_output(&card)?;
    let (w, h) = output.mode.size();
    let (w, h) = (w as u32, h as u32);

    let setup = Instant::now();
    // BGRA bytes in memory are XRGB8888 on little-endian, so rows copy straight over.
    let mut target = Offscreen::new(w, h, wgpu::TextureFormat::Bgra8UnormSrgb)?;
    let gpu_setup = setup.elapsed();
    let mut buffers = Vec::new();
    for _ in 0..o.buffers {
        buffers.push(scanout(&card, w, h)?);
    }

    let mut scene = Scene::new();
    scene.apply_all(compositor::demo::build(w as f32, h as f32, o.windows));

    let mut out = vec![
        format!(
            "DRM driver: {} ({})",
            driver.name().to_string_lossy(),
            driver.description().to_string_lossy()
        ),
        format!("Output: {} at {}x{} @ {} Hz", output.name, w, h, output.mode.vrefresh()),
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
    let mut pending = false;
    for frame in 0..o.frames.max(1) {
        let frame_start = Instant::now();
        let mut flip_wait = std::time::Duration::ZERO;
        // Double buffering: the free buffer is on screen until the pending flip lands.
        if o.buffers < 3 && pending {
            let wait_start = Instant::now();
            wait_for_flip(&card)?;
            flip_wait += wait_start.elapsed();
            pending = false;
        }
        animate(&mut scene, frame, &hud);
        let back = &mut buffers[frame % o.buffers];
        let pitch = back.buffer.pitch() as usize;

        // Render and read back into a staging Vec, then copy into the mapped dumb
        // buffer, so the two costs show up separately.
        let render_start = Instant::now();
        let mut rows: Vec<u8> = Vec::with_capacity((w * h * 4) as usize);
        target.render(&scene.draw_list(), 1.0, Rgba::BLACK, |row| rows.extend_from_slice(row))?;
        t.render.push(render_start.elapsed());

        let copy_start = Instant::now();
        {
            let mut map =
                card.map_dumb_buffer(&mut back.buffer).map_err(|e| format!("map dumb buffer: {e}"))?;
            let dst = map.as_mut();
            let row_bytes = (w * 4) as usize;
            for (y, src) in rows.chunks(row_bytes).enumerate() {
                dst[y * pitch..y * pitch + row_bytes].copy_from_slice(src);
            }
        }
        t.copy.push(copy_start.elapsed());

        // Only one flip may be queued at a time.
        if pending {
            let wait_start = Instant::now();
            wait_for_flip(&card)?;
            flip_wait += wait_start.elapsed();
        }
        if frame == 0 {
            card.set_crtc(output.crtc, Some(back.fb), (0, 0), &[output.connector], Some(output.mode))
                .map_err(|e| format!("set CRTC (is another process DRM master?): {e}"))?;
            pending = false;
        } else {
            card.page_flip(output.crtc, back.fb, PageFlipFlags::EVENT, None)
                .map_err(|e| format!("page flip: {e}"))?;
            pending = true;
        }
        t.flip_wait.push(flip_wait);
        t.frame.push(frame_start.elapsed());
        if frame % 30 == 29 {
            let recent = &t.frame[t.frame.len() - 30..];
            let avg = recent.iter().sum::<std::time::Duration>().as_secs_f64() / 30.0;
            hud = format!("kms-probe  {:.1} ms/frame  {:.0} fps", avg * 1000.0, 1.0 / avg);
        }
    }
    if pending {
        wait_for_flip(&card)?;
    }
    out.extend(t.report(started));

    if !o.hold.is_zero() {
        // tools/spike-kms waits for this line before capturing the scanout.
        eprintln!("kms-probe: holding the last frame for {} s", o.hold.as_secs());
        std::thread::sleep(o.hold);
    }
    for b in buffers {
        let _ = card.destroy_framebuffer(b.fb);
        let _ = card.destroy_dumb_buffer(b.buffer);
    }
    Ok(out)
}
