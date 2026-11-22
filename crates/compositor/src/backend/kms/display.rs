//! A KMS display: the connected output, its scanout buffers, and page flips.
//!
//! With two buffers the next frame cannot start until the pending flip completes,
//! because the only free buffer is still on screen. With three, the next frame
//! renders while the flip is pending, so work that nearly fills a vblank period no
//! longer costs a whole extra period.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, Instant};

use drm::Device as _;
use drm::buffer::{Buffer as _, DrmFourcc};
use drm::control::{
    Device as ControlDevice, Event, Mode, ModeTypeFlags, PageFlipFlags, connector, crtc,
    dumbbuffer::DumbBuffer, framebuffer,
};

/// Pixel format of the scanout buffers. BGRA bytes in memory are XRGB8888 on
/// little-endian, so a `Bgra8UnormSrgb` texture's rows copy straight over.
pub const RENDER_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

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

pub struct Display {
    card: Card,
    output: Output,
    buffers: Vec<Scanout>,
    /// The buffer the next frame is drawn into: neither on screen nor pending.
    back: usize,
    /// A page flip is queued and its event has not arrived yet.
    pending: bool,
    /// The CRTC has been set, so later frames page-flip.
    shown: bool,
    driver: String,
}

impl Display {
    /// Open `card` (e.g. `/dev/dri/card0`), pick the first connected output and its
    /// preferred mode, and allocate `buffers` (2 or 3) scanout buffers.
    pub fn open(card: &str, buffers: usize) -> Result<Display, String> {
        assert!((2..=3).contains(&buffers), "2 or 3 scanout buffers");
        let file =
            OpenOptions::new().read(true).write(true).open(card).map_err(|e| format!("{card}: {e}"))?;
        let card = Card(file);
        let driver = card.get_driver().map_err(|e| format!("get driver: {e}"))?;
        let driver =
            format!("{} ({})", driver.name().to_string_lossy(), driver.description().to_string_lossy());
        let output = find_output(&card)?;
        let mut display =
            Display { card, output, buffers: Vec::new(), back: 0, pending: false, shown: false, driver };
        let (w, h) = display.size();
        for _ in 0..buffers {
            let buffer = display
                .card
                .create_dumb_buffer((w, h), DrmFourcc::Xrgb8888, 32)
                .map_err(|e| format!("create dumb buffer: {e}"))?;
            let fb =
                display.card.add_framebuffer(&buffer, 24, 32).map_err(|e| format!("add framebuffer: {e}"))?;
            display.buffers.push(Scanout { buffer, fb });
        }
        Ok(display)
    }

    pub fn size(&self) -> (u32, u32) {
        let (w, h) = self.output.mode.size();
        (w as u32, h as u32)
    }

    pub fn refresh_hz(&self) -> u32 {
        self.output.mode.vrefresh()
    }

    /// Connector name, e.g. `Virtual-1` or `HDMIA-1`.
    pub fn output_name(&self) -> &str {
        &self.output.name
    }

    /// DRM driver name and description, e.g. `virtio_gpu (virtio GPU)`.
    pub fn driver(&self) -> &str {
        &self.driver
    }

    pub fn buffer_count(&self) -> usize {
        self.buffers.len()
    }

    /// Copy tightly packed rows (`width * 4` bytes each, [`RENDER_FORMAT`]) into the
    /// back buffer. With two buffers this first waits for any pending flip, because
    /// the back buffer is the one still on screen until then.
    pub fn copy_into_back(&mut self, rows: &[u8]) -> Result<(), String> {
        if self.buffers.len() < 3 {
            self.wait_for_flip()?;
        }
        let (w, _) = self.size();
        let row_bytes = (w * 4) as usize;
        let back = &mut self.buffers[self.back];
        let pitch = back.buffer.pitch() as usize;
        let mut map =
            self.card.map_dumb_buffer(&mut back.buffer).map_err(|e| format!("map dumb buffer: {e}"))?;
        let dst = map.as_mut();
        for (y, src) in rows.chunks(row_bytes).enumerate() {
            dst[y * pitch..y * pitch + row_bytes].copy_from_slice(src);
        }
        Ok(())
    }

    /// Block until the pending page flip (if any) has happened, i.e. until vblank.
    /// Returns how long it waited.
    pub fn wait_for_flip(&mut self) -> Result<Duration, String> {
        let started = Instant::now();
        while self.pending {
            for event in self.card.receive_events().map_err(|e| format!("receive events: {e}"))? {
                if let Event::PageFlip(_) = event {
                    self.pending = false;
                }
            }
        }
        Ok(started.elapsed())
    }

    /// Show the back buffer: set the CRTC on the first frame, page-flip after that.
    /// Only one flip may be queued, so this first waits for the previous one.
    /// Returns how long it waited.
    pub fn flip(&mut self) -> Result<Duration, String> {
        let waited = self.wait_for_flip()?;
        let fb = self.buffers[self.back].fb;
        if self.shown {
            self.card
                .page_flip(self.output.crtc, fb, PageFlipFlags::EVENT, None)
                .map_err(|e| format!("page flip: {e}"))?;
            self.pending = true;
        } else {
            self.card
                .set_crtc(
                    self.output.crtc,
                    Some(fb),
                    (0, 0),
                    &[self.output.connector],
                    Some(self.output.mode),
                )
                .map_err(|e| format!("set CRTC (is another process DRM master?): {e}"))?;
            self.shown = true;
        }
        self.back = (self.back + 1) % self.buffers.len();
        Ok(waited)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.wait_for_flip();
        for b in std::mem::take(&mut self.buffers) {
            let _ = self.card.destroy_framebuffer(b.fb);
            let _ = self.card.destroy_dumb_buffer(b.buffer);
        }
    }
}
