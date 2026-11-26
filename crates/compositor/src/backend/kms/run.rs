//! The KMS backend's frame loop.
//!
//! One thread waits in `poll()` on the input devices and the DRM device (for
//! page-flip events) until the frame-time readout is due. Input is applied to the
//! scene as it arrives; a frame is drawn only when the scene changed. All pending
//! input is read before drawing, so a fast drag costs one frame, not one per event.
//! A stop signal ends the loop normally, so the console, cursor and buffers are put
//! back as their owners drop.
//!
//! Requests from apps arrive on the bus thread, which writes a byte to a socket
//! pair that's in the same `poll()`, so they're handled as promptly as input.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::console::{self, Console};
use super::display::{Display, RENDER_FORMAT};
use super::input::Devices;
use super::signals::Signals;
use crate::backend::{Options, Scanout};
use crate::bus;
use crate::damage::DamageTracker;
use crate::desktop::Desktop;
use crate::headless::Offscreen;
use crate::input::{Effect, Seat};
use crate::scene::{Rgba, Scene};
use crate::stats::FrameTimer;
use crate::{cursor, demo};

/// Scanout buffers. Three let a frame render while the previous flip is pending.
const BUFFERS: usize = 3;

pub fn run(opts: Options) -> Result<(), Box<dyn std::error::Error>> {
    // First, before wgpu starts any threads; see Signals::block.
    let signals = Signals::block()?;
    let mut display = Display::open(opts.card.as_deref(), BUFFERS)?;
    let mode = display.size();
    let mut target = Offscreen::new(mode.0, mode.1, RENDER_FORMAT)?;
    // Render fewer pixels and let the display scale them up, if asked.
    let cpu = target.adapter.device_type == wgpu::DeviceType::Cpu;
    let want = opts.render_height.size(mode, cpu);
    if want != mode {
        match display.scale_from(want) {
            Ok(()) => target.resize(want.0, want.1),
            Err(e) => log::warn!("kms: {e}; rendering at {}x{}", mode.0, mode.1),
        }
    }
    let (w, h) = display.size();
    // Direct scanout: the GPU renders straight into the display's buffers, shared as
    // dma-bufs, with no readback or copy. Not for a CPU renderer, whose frames are in
    // ordinary memory anyway, and only where the driver can import the buffers.
    let mut scanout = None;
    if opts.scanout == Scanout::Direct || (opts.scanout == Scanout::Auto && !cpu) {
        let imported = display.export_buffers().and_then(|bufs| {
            bufs.into_iter()
                .map(|(fd, pitch)| target.import_dmabuf(fd, w, h, pitch))
                .collect::<Result<Vec<_>, _>>()
        });
        match imported {
            Ok(views) => {
                // The offscreen texture and readback buffer go unused.
                target.resize(1, 1);
                scanout = Some(views);
            }
            Err(e) if opts.scanout == Scanout::Direct => return Err(format!("direct scanout: {e}").into()),
            Err(e) => log::warn!("kms: direct scanout unavailable ({e}); copying frames instead"),
        }
    }
    let path = if scanout.is_some() { "direct" } else { "copy" };
    // tools/smoke-checks.ts reads the frame size from "at WxH @".
    let scaled = if (w, h) == mode { String::new() } else { format!(", scaled up to {}x{}", mode.0, mode.1) };
    log::info!(
        "kms: {} on {} at {w}x{h} @ {} Hz{scaled}; {path} scanout; adapter {} ({:?})",
        display.driver(),
        display.output_name(),
        display.refresh_hz(),
        target.adapter.name,
        target.adapter.backend,
    );
    // Declared after the GPU, so it is dropped (and the console restored) before the
    // GPU is torn down: a driver that crashes in teardown (Venus under UTM did) must
    // not leave the console's keyboard off. The kernel releases the display and its
    // buffers when the process dies, whatever happens.
    let _console = match Console::mute(console::FOREGROUND) {
        Ok(c) => {
            log::info!("console: keyboard and text output off on {}", console::FOREGROUND);
            Some(c)
        }
        Err(e) => {
            log::warn!("console: {e}; keys typed into the desktop also reach the text console");
            None
        }
    };

    // Without input the desktop still draws; say why and carry on.
    let mut devices = match Devices::open("seat0") {
        Ok(devices) => Some(devices),
        Err(e) => {
            log::warn!("input: {e}; continuing without input");
            None
        }
    };
    let mut inputs = Vec::new();
    if let Some(d) = &mut devices {
        d.read((w, h), &mut inputs)?; // logs the devices found at startup
    }

    // After Signals::block, so the bus thread inherits the blocked mask.
    let (mut woken, wake) = UnixStream::pair()?;
    woken.set_nonblocking(true)?;
    wake.set_nonblocking(true)?;
    let bridge = bus::connect(move || {
        // A full socket buffer already means "wake up"; nothing more to say.
        let _ = (&wake).write(&[1]);
    });

    let background = Rgba::parse(demo::BACKGROUND).expect("valid color");
    let mut desktop = Desktop::new(w as f32, h as f32);
    desktop.scene.set_dark(opts.dark);
    desktop.scene.apply_all(demo::build(w as f32, h as f32, opts.windows));
    desktop.set_screen(w as f32, h as f32);
    let arrow = cursor::arrow();
    let mut timer = FrameTimer::default();
    let mut next_hud = Instant::now();
    // Only the copy path uses this; it grows to a frame's size on the first frame.
    let mut rows: Vec<u8> = Vec::new();
    // What changed since each buffer was drawn: the scanout buffers in turn, or on the
    // copy path the one offscreen texture, which always holds the last frame.
    let mut damage = DamageTracker::new(if scanout.is_some() { display.buffer_count() } else { 1 });
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
            desktop.scene.apply(demo::hud(&summary));
            timer.reset_window();
            next_hud = now + Duration::from_secs(1);
        }

        // Coasting scroll views, the window switcher's timeout, the clock; then
        // layout, which can change what's drawn.
        desktop.tick(display.next_vblank());
        desktop.relayout();
        let mut drew = false;
        if desktop.scene.is_dirty() || opts.continuous {
            let started = Instant::now();
            let list = desktop.draw_list();
            let screen = (w as f32, h as f32);
            let changed = damage.next_frame(&list, screen);
            // A change that draws nothing different (an app re-setting a value) costs
            // no frame. --continuous measures full redraws, so it always draws them.
            if opts.continuous || !changed.as_ref().is_some_and(Vec::is_empty) {
                let buffer = if scanout.is_some() { display.back_index() } else { 0 };
                let region = if opts.continuous { None } else { damage.region_for(buffer, screen) };
                let region = region.as_deref();
                let stats = if let Some(views) = &scanout {
                    // Three buffers: the back one is never on screen or pending.
                    target.render_to(&views[buffer], (w, h), &list, 1.0, background, region)?
                } else {
                    rows.clear();
                    let stats =
                        target.render(&list, 1.0, background, region, |r| rows.extend_from_slice(r))?;
                    display.copy_into_back(&rows)?;
                    stats
                };
                damage.drew(buffer);
                // Drawing, plus readback and copy on the copy path: a frame's cost.
                timer.record(started.elapsed(), stats.quads, stats.texts);
                display.flip()?;
                drew = true;
            }
            desktop.scene.clear_dirty();
            if first && drew {
                // tools/qemu-smoke.ts waits for this line before capturing the display.
                log::info!("kms: first frame on screen");
                first = false;
                // The cursor needs the CRTC that the first flip set up.
                match display.show_cursor(&arrow) {
                    Ok(()) => display.move_cursor(desktop.seat.cursor().0, desktop.seat.cursor().1)?,
                    Err(e) => log::warn!("kms: {e}; the pointer will be invisible"),
                }
            }
        }

        // Sleep until input, a page flip or the readout, unless there is more to draw.
        let timeout = if desktop.scene.is_dirty() || opts.continuous {
            Duration::ZERO
        } else {
            let now = Instant::now();
            next_hud.min(desktop.next_wake(now)).saturating_duration_since(now)
        };
        let flip_fd = display.flip_pending().then(|| display.fd());
        let ready = wait(devices.as_ref().map(Devices::fd), flip_fd, signals.fd(), woken.as_fd(), timeout)?;
        if ready.stop
            && let Some(name) = signals.take()
        {
            log::info!("kms: stopping on {name}");
            return Ok(());
        }
        if ready.display {
            display.dispatch_events()?;
        }
        if ready.input
            && let Some(d) = &mut devices
        {
            d.read((w, h), &mut inputs)?;
            for input in inputs.drain(..) {
                let (effect, outs) = desktop.input(input);
                if let Some(b) = &bridge {
                    b.send(outs);
                }
                report(&effect, &desktop.scene, &display, &desktop.seat)?;
            }
        }
        if ready.wake {
            let mut sink = [0u8; 256];
            while matches!(woken.read(&mut sink), Ok(n) if n > 0) {}
            if let Some(b) = &bridge {
                for call in b.calls() {
                    b.send(desktop.call(call));
                }
            }
        }
    }
}

/// Act on what one input did: move the hardware cursor and log window actions.
fn report(effect: &Effect, scene: &Scene, display: &Display, seat: &Seat) -> Result<(), String> {
    if effect.cursor_moved {
        display.move_cursor(seat.cursor().0, seat.cursor().1)?;
    }
    // One line per finished action, not per motion; tools/smoke-checks.ts reads these.
    if let Some(id) = effect.dropped
        && let Some(r) = scene.local_rect(id)
    {
        log::info!("wm: dropped window {id} at ({}, {})", r.x, r.y);
    }
    if let Some(id) = effect.raised {
        log::info!("wm: raised window {id} from the keyboard");
    }
    Ok(())
}

struct Ready {
    input: bool,
    display: bool,
    stop: bool,
    wake: bool,
}

/// `poll()` the input and DRM fds (either may be absent), the stop-signal fd and
/// the bus thread's wake-up socket for up to `timeout`.
fn wait(
    input: Option<BorrowedFd>,
    display: Option<BorrowedFd>,
    stop: BorrowedFd,
    wake: BorrowedFd,
    timeout: Duration,
) -> Result<Ready, String> {
    // poll() skips entries with a negative fd.
    let entry = |fd: Option<BorrowedFd>| libc::pollfd {
        fd: fd.map_or(-1, |f| f.as_raw_fd()),
        events: libc::POLLIN,
        revents: 0,
    };
    let mut fds = [entry(input), entry(display), entry(Some(stop)), entry(Some(wake))];
    // Round up, so a wait shorter than a millisecond doesn't spin.
    let ms = timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
    // SAFETY: `fds` is a valid array of pollfd structs for the whole call.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(Ready { input: false, display: false, stop: false, wake: false });
        }
        return Err(format!("poll: {e}"));
    }
    let readable = |p: &libc::pollfd| p.revents & libc::POLLIN != 0;
    Ok(Ready {
        input: readable(&fds[0]),
        display: readable(&fds[1]),
        stop: readable(&fds[2]),
        wake: readable(&fds[3]),
    })
}
