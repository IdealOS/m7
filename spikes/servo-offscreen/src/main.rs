//! Spike E: Servo embedded with no window, as `browser-host` would run it.
//!
//! Loads a page into a webview drawn offscreen (Servo's `SoftwareRenderingContext`,
//! GL through Mesa), saves what it drew, scrolls it with a forwarded wheel event and
//! saves that too. Then asks EGL whether the frame can leave the process as a dmabuf,
//! which is how the compositor would show it, and checks the exported buffer holds the
//! same pixels. Prints a Markdown report.
//!
//! `servo-offscreen URL [OUT_PREFIX] [WIDTHxHEIGHT]`

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use servo::{
    DeviceIntRect, DevicePoint, EventLoopWaker, InputEvent, LoadStatus, RenderingContext, RgbaImage,
    ServoBuilder, SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate, WebViewPoint,
    WheelDelta, WheelEvent, WheelMode,
};

/// Wakes the main thread when Servo has work for it (it may call from any thread).
#[derive(Clone, Default)]
struct Waker(Arc<(Mutex<bool>, Condvar)>);

impl Waker {
    fn wait(&self, timeout: Duration) {
        let (lock, cv) = &*self.0;
        let mut woken = lock.lock().unwrap();
        if !*woken {
            woken = cv.wait_timeout(woken, timeout).unwrap().0;
        }
        *woken = false;
    }
}

impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {
        let (lock, cv) = &*self.0;
        *lock.lock().unwrap() = true;
        cv.notify_one();
    }
}

#[derive(Default)]
struct Page {
    frame_ready: Cell<bool>,
    complete: Cell<bool>,
    title: RefCell<Option<String>>,
}

impl WebViewDelegate for Page {
    fn notify_new_frame_ready(&self, _: WebView) {
        self.frame_ready.set(true);
    }
    fn notify_load_status_changed(&self, _: WebView, status: LoadStatus) {
        if matches!(status, LoadStatus::Complete) {
            self.complete.set(true);
        }
    }
    fn notify_page_title_changed(&self, _: WebView, title: Option<String>) {
        *self.title.borrow_mut() = title;
    }
}

enum Phase {
    Loading,
    /// Loaded; waiting for late images and layout to settle.
    Settling(Instant),
    /// The wheel event went in at this instant.
    Scrolling(Instant),
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    let url = url::Url::parse(args.get(1).map_or("https://example.com", String::as_str))?;
    let out = args.get(2).cloned().unwrap_or_else(|| "page".into());
    let (w, h) = args
        .get(3)
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((1280u32, 800u32));

    rustls::crypto::aws_lc_rs::default_provider().install_default().map_err(|_| "crypto provider")?;

    let t0 = Instant::now();
    let rc = Rc::new(SoftwareRenderingContext::new(PhysicalSize::new(w, h)).map_err(|e| format!("{e:?}"))?);
    rc.make_current().map_err(|e| format!("{e:?}"))?;
    let gl = rc.gleam_gl_api();
    let renderer = gl.get_string(gleam::gl::RENDERER);
    let version = gl.get_string(gleam::gl::VERSION);
    let waker = Waker::default();
    let servo = ServoBuilder::default().event_loop_waker(Box::new(waker.clone())).build();
    let started = t0.elapsed();

    let page = Rc::new(Page::default());
    let loading = Instant::now();
    let webview = WebViewBuilder::new(&servo, rc.clone()).url(url.clone()).delegate(page.clone()).build();

    let mut phase = Phase::Loading;
    let mut paints: Vec<Duration> = Vec::new();
    let mut first_frame = None;
    let mut loaded = None;
    let mut last_frame = Instant::now();
    let mut last_image: Option<RgbaImage> = None;
    let mut loaded_rss = None;
    let mut scroll_latency = None;
    let mut dmabuf = None;
    let full = DeviceIntRect::from_size(rc.size2d().to_i32());

    loop {
        waker.wait(Duration::from_millis(20));
        servo.spin_event_loop();
        let now = Instant::now();
        if page.frame_ready.take() {
            let p = Instant::now();
            webview.paint();
            paints.push(p.elapsed());
            first_frame.get_or_insert(loading.elapsed());
            last_frame = Instant::now();
            if let Phase::Scrolling(sent) = phase {
                scroll_latency.get_or_insert(last_frame - sent);
            }
            // Read back now, while this frame's buffer is bound.
            last_image = rc.read_to_image(full);
            if dmabuf.is_none() && page.complete.get() {
                dmabuf = Some(export_dmabuf(&*gl, last_image.as_ref()));
            }
            rc.present();
        }
        let quiet = now - last_frame > Duration::from_millis(400);
        match phase {
            Phase::Loading if page.complete.get() => {
                loaded = Some(loading.elapsed());
                phase = Phase::Settling(now);
            }
            Phase::Settling(since)
                if (now - since > Duration::from_secs(1) && quiet)
                    || now - since > Duration::from_secs(8) =>
            {
                save(&last_image, &format!("{out}.png"))?;
                loaded_rss = Some(memory());
                let delta = WheelDelta { x: 0.0, y: -600.0, z: 0.0, mode: WheelMode::DeltaPixel };
                let at = WebViewPoint::Device(DevicePoint::new(w as f32 / 2.0, h as f32 / 2.0));
                webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(delta, at)));
                phase = Phase::Scrolling(Instant::now());
            }
            Phase::Scrolling(sent)
                if (scroll_latency.is_some() && quiet) || now - sent > Duration::from_secs(5) =>
            {
                save(&last_image, &format!("{out}-scrolled.png"))?;
                break;
            }
            _ => {}
        }
        if t0.elapsed() > Duration::from_secs(90) {
            eprintln!("gave up after 90 s");
            save(&last_image, &format!("{out}.png"))?;
            break;
        }
    }

    let ms = |d: Option<Duration>| d.map_or("-".into(), |d| format!("{:.0} ms", d.as_secs_f64() * 1000.0));
    let mut sorted = paints.clone();
    sorted.sort();
    let (hwm, rss) = memory();
    println!("## Spike E: Servo offscreen\n");
    println!("| | |\n| --- | --- |");
    println!("| Page | {url} |");
    println!("| Title | {} |", page.title.borrow().clone().unwrap_or_default());
    println!("| Size | {w}x{h} |");
    println!("| GL | {renderer} ({version}) |");
    println!("| Servo started | {} |", ms(Some(started)));
    println!("| First frame | {} after load began |", ms(first_frame));
    println!("| Loaded (`readyState` complete) | {} |", ms(loaded));
    println!(
        "| Paints | {} (median {}, slowest {}) |",
        paints.len(),
        ms(sorted.get(sorted.len() / 2).copied()),
        ms(sorted.last().copied())
    );
    println!("| Wheel event to next frame | {} |", ms(scroll_latency));
    if let Some((h, r)) = loaded_rss {
        println!("| Memory when loaded | {} MB resident (peak {} MB) |", r / 1024, h / 1024);
    }
    println!("| Memory at exit | {} MB resident (peak {} MB) |", rss / 1024, hwm / 1024);
    println!(
        "| Frame as a dmabuf | {} |",
        dmabuf.unwrap_or_else(|| "not tried (no frame after load)".into())
    );
    Ok(())
}

fn save(image: &Option<RgbaImage>, path: &str) -> Result<(), Box<dyn Error>> {
    match image {
        Some(i) => Ok(i.save(path)?),
        None => Err(format!("no frame to save as {path}").into()),
    }
}

/// (Peak, current) resident memory in KB.
fn memory() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    (field("VmHWM:"), field("VmRSS:"))
}

type QueryFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut i32, *mut i32, *mut u64) -> u32;
type ExportFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut i32, *mut i32, *mut i32) -> u32;

/// Whether the frame just drawn (the bound framebuffer's colour attachment) can be
/// exported as a dmabuf, and whether the exported buffer holds what `read_to_image`
/// read. The compositor would import such a buffer as a texture without a copy.
fn export_dmabuf(gl: &dyn gleam::gl::Gl, readback: Option<&RgbaImage>) -> String {
    let egl = match unsafe { khronos_egl::DynamicInstance::<khronos_egl::EGL1_5>::load_required() } {
        Ok(egl) => egl,
        Err(e) => return format!("no EGL 1.5: {e}"),
    };
    let (Some(display), Some(context)) = (egl.get_current_display(), egl.get_current_context()) else {
        return "no current EGL display".into();
    };
    let extensions = egl
        .query_string(Some(display), khronos_egl::EXTENSIONS)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !extensions.contains("EGL_MESA_image_dma_buf_export") {
        return "no: EGL lacks EGL_MESA_image_dma_buf_export".into();
    }
    let mut fbo = [0];
    unsafe { gl.get_integer_v(gleam::gl::FRAMEBUFFER_BINDING, &mut fbo) };
    let fbo = fbo[0];
    let kind = gl.get_framebuffer_attachment_parameter_iv(
        gleam::gl::FRAMEBUFFER,
        gleam::gl::COLOR_ATTACHMENT0,
        gleam::gl::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,
    ) as u32;
    let name = gl.get_framebuffer_attachment_parameter_iv(
        gleam::gl::FRAMEBUFFER,
        gleam::gl::COLOR_ATTACHMENT0,
        gleam::gl::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME,
    );
    let target = match kind {
        gleam::gl::TEXTURE => 0x30B1,      // EGL_GL_TEXTURE_2D
        gleam::gl::RENDERBUFFER => 0x30B9, // EGL_GL_RENDERBUFFER
        other => return format!("no: framebuffer {fbo}'s attachment is of type {other:#x}"),
    };
    let buffer = unsafe { khronos_egl::ClientBuffer::from_ptr(name as usize as *mut c_void) };
    gl.finish();
    let image = match egl.create_image(display, context, target, buffer, &[khronos_egl::ATTRIB_NONE]) {
        Ok(i) => i,
        Err(e) => return format!("no: eglCreateImage failed ({e})"),
    };
    let result = (|| {
        let query: QueryFn = unsafe { std::mem::transmute(egl.get_proc_address("eglExportDMABUFImageQueryMESA")?) };
        let export: ExportFn = unsafe { std::mem::transmute(egl.get_proc_address("eglExportDMABUFImageMESA")?) };
        let (mut fourcc, mut planes, mut modifier) = (0i32, 0i32, 0u64);
        if unsafe { query(display.as_ptr(), image.as_ptr(), &mut fourcc, &mut planes, &mut modifier) } == 0 {
            return Some("no: eglExportDMABUFImageQueryMESA failed".into());
        }
        let (mut fds, mut strides, mut offsets) = ([-1i32; 4], [0i32; 4], [0i32; 4]);
        if unsafe { export(display.as_ptr(), image.as_ptr(), fds.as_mut_ptr(), strides.as_mut_ptr(), offsets.as_mut_ptr()) } == 0 {
            return Some("no: eglExportDMABUFImageMESA failed".into());
        }
        let code: String = fourcc.to_le_bytes().iter().map(|&b| b as char).collect();
        if fds[0] < 0 {
            // llvmpipe does this: its buffers are plain memory with nothing to share.
            return Some(format!("no: EGL describes it ({code}, modifier {modifier:#x}) but the driver gives no file descriptor"));
        }
        let check = compare(fds[0], strides[0], offsets[0], readback);
        Some(format!(
            "yes: {planes} plane(s), format {code}, modifier {modifier:#x}, stride {}; {check}",
            strides[0]
        ))
    })()
    .unwrap_or_else(|| "no: export functions missing".into());
    let _ = egl.destroy_image(display, image);
    result
}

/// Map the exported buffer and compare it with the read-back frame (rows may be
/// upside down: GL's origin is the bottom left).
fn compare(fd: i32, stride: i32, offset: i32, readback: Option<&RgbaImage>) -> String {
    let Some(img) = readback else { return "nothing to compare".into() };
    let (w, h) = img.dimensions();
    let len = offset as usize + stride as usize * h as usize;
    let map = unsafe { libc_mmap(fd, len) };
    let Some(bytes) = map else {
        return format!("could not map it ({}, fd {fd}, {len} bytes)", std::io::Error::last_os_error());
    };
    let row = |y: u32| &bytes[offset as usize + stride as usize * y as usize..][..w as usize * 4];
    let differs = |flip: bool| {
        (0..h)
            .step_by(37)
            .filter(|&y| {
                let src = img.as_raw()[(y * w * 4) as usize..][..(w * 4) as usize].to_vec();
                let dst = row(if flip { h - 1 - y } else { y });
                // The buffer may be BGRA or RGBA: compare ignoring the red/blue order.
                src.chunks(4)
                    .zip(dst.chunks(4))
                    .any(|(a, b)| !(a == b || (a[0] == b[2] && a[1] == b[1] && a[2] == b[0])))
            })
            .count()
    };
    let verdict = if differs(false) == 0 {
        "its pixels match the read-back frame".into()
    } else if differs(true) == 0 {
        "its pixels match the read-back frame, upside down".into()
    } else {
        "its pixels differ from the read-back frame".into()
    };
    unsafe { libc_munmap(bytes) };
    verdict
}

unsafe extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
}

unsafe fn libc_mmap(fd: i32, len: usize) -> Option<&'static [u8]> {
    const PROT_READ: i32 = 1;
    const MAP_SHARED: i32 = 1;
    let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_SHARED, fd, 0) };
    if p as isize == -1 { None } else { Some(unsafe { std::slice::from_raw_parts(p as *const u8, len) }) }
}

unsafe fn libc_munmap(bytes: &[u8]) {
    unsafe { munmap(bytes.as_ptr() as *mut c_void, bytes.len()) };
}
