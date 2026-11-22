//! A KMS display: the connected output, its scanout buffers, and page flips.
//!
//! With two buffers the next frame cannot start until the pending flip completes,
//! because the only free buffer is still on screen. With three, the next frame
//! renders while the flip is pending, so work that nearly fills a vblank period no
//! longer costs a whole extra period.
//!
//! The pointer is a hardware cursor, so moving it never redraws the scene.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, Instant};

use crate::cursor::Bitmap;

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
    /// The cursor image, once [`Display::show_cursor`] has succeeded.
    cursor: Option<Cursor>,
}

struct Cursor {
    buffer: DumbBuffer,
    hotspot: (i32, i32),
}

/// Side of the cursor buffer. 64x64 is the size every KMS driver supports.
const CURSOR_SIZE: u32 = 64;

impl Display {
    /// Open `card` (e.g. `/dev/dri/card1`), or with `None` the first card that has a
    /// connected display, and allocate `buffers` (2 or 3) scanout buffers.
    ///
    /// A Pi 4 has two DRM cards: `v3d` renders but has no outputs, and `vc4` drives
    /// HDMI. Their numbering depends on probe order, so search rather than assume.
    pub fn open(card: Option<&str>, buffers: usize) -> Result<Display, String> {
        if let Some(card) = card {
            return Display::open_card(card, buffers);
        }
        let mut cards: Vec<String> = std::fs::read_dir("/dev/dri")
            .map_err(|e| format!("/dev/dri: {e}"))?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|name| name.starts_with("card"))
            .map(|name| format!("/dev/dri/{name}"))
            .collect();
        cards.sort_by_key(|path| card_number(path));
        let mut errors = Vec::new();
        for card in &cards {
            match Display::open_card(card, buffers) {
                Ok(display) => return Ok(display),
                Err(e) => errors.push(e),
            }
        }
        if errors.is_empty() {
            Err("no DRM cards in /dev/dri".into())
        } else {
            Err(format!("no card with a connected display: {}", errors.join("; ")))
        }
    }

    fn open_card(path: &str, buffers: usize) -> Result<Display, String> {
        assert!((2..=3).contains(&buffers), "2 or 3 scanout buffers");
        let file =
            OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
        let card = Card(file);
        let driver = card.get_driver().map_err(|e| format!("{path}: get driver: {e}"))?;
        let driver =
            format!("{} ({})", driver.name().to_string_lossy(), driver.description().to_string_lossy());
        let output = find_output(&card).map_err(|e| format!("{path} ({driver}): {e}"))?;
        let mut display = Display {
            card,
            output,
            buffers: Vec::new(),
            back: 0,
            pending: false,
            shown: false,
            driver,
            cursor: None,
        };
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

    /// The DRM device, to poll for page-flip events alongside other input.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.card.as_fd()
    }

    /// A page flip is queued and has not completed.
    pub fn flip_pending(&self) -> bool {
        self.pending
    }

    /// Read the DRM events waiting on [`Display::fd`]. Blocks if there are none, so
    /// call it only once the fd is readable or a flip is pending.
    pub fn dispatch_events(&mut self) -> Result<(), String> {
        for event in self.card.receive_events().map_err(|e| format!("receive events: {e}"))? {
            if let Event::PageFlip(_) = event {
                self.pending = false;
            }
        }
        Ok(())
    }

    /// Block until the pending page flip (if any) has happened, i.e. until vblank.
    /// Returns how long it waited.
    pub fn wait_for_flip(&mut self) -> Result<Duration, String> {
        let started = Instant::now();
        while self.pending {
            self.dispatch_events()?;
        }
        Ok(started.elapsed())
    }

    /// Show `image` as the hardware cursor. Call after the first [`Display::flip`]:
    /// a cursor needs an active CRTC.
    //
    // The legacy cursor ioctls are deprecated in the drm crate in favor of cursor
    // planes, but the kernel applies legacy cursor updates asynchronously. Moving a
    // cursor plane with SETPLANE waits for vblank on atomic drivers such as vc4, which
    // would stall the frame loop on every pointer motion. Atomic commits with the
    // cursor plane are the long-term path.
    #[allow(deprecated)]
    pub fn show_cursor(&mut self, image: &Bitmap) -> Result<(), String> {
        assert!(image.width <= CURSOR_SIZE && image.height <= CURSOR_SIZE, "cursor fits the buffer");
        assert_eq!(image.pixels.len(), (image.width * image.height) as usize);
        let mut buffer = self
            .card
            .create_dumb_buffer((CURSOR_SIZE, CURSOR_SIZE), DrmFourcc::Argb8888, 32)
            .map_err(|e| format!("create cursor buffer: {e}"))?;
        let pitch = buffer.pitch() as usize;
        {
            let mut map =
                self.card.map_dumb_buffer(&mut buffer).map_err(|e| format!("map cursor buffer: {e}"))?;
            let dst = map.as_mut();
            dst.fill(0);
            for (y, row) in image.pixels.chunks(image.width as usize).enumerate() {
                for (x, px) in row.iter().enumerate() {
                    dst[y * pitch + x * 4..][..4].copy_from_slice(&px.to_le_bytes());
                }
            }
        }
        let hotspot = (image.hotspot.0 as i32, image.hotspot.1 as i32);
        if let Err(e) = self.card.set_cursor2(self.output.crtc, Some(&buffer), hotspot) {
            let _ = self.card.destroy_dumb_buffer(buffer);
            return Err(format!("set cursor (no hardware cursor on this output?): {e}"));
        }
        if let Some(old) = self.cursor.replace(Cursor { buffer, hotspot }) {
            let _ = self.card.destroy_dumb_buffer(old.buffer);
        }
        Ok(())
    }

    /// Put the cursor's hotspot at `(x, y)`. Does nothing before `show_cursor`.
    #[allow(deprecated)] // see show_cursor
    pub fn move_cursor(&self, x: f32, y: f32) -> Result<(), String> {
        let Some(cursor) = &self.cursor else { return Ok(()) };
        let pos = (x as i32 - cursor.hotspot.0, y as i32 - cursor.hotspot.1);
        self.card.move_cursor(self.output.crtc, pos).map_err(|e| format!("move cursor: {e}"))
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

/// `/dev/dri/card10` sorts after `card9`.
fn card_number(path: &str) -> u32 {
    path.trim_start_matches("/dev/dri/card").parse().unwrap_or(u32::MAX)
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.wait_for_flip();
        if let Some(cursor) = self.cursor.take() {
            #[allow(deprecated)] // see show_cursor
            let _ = self.card.set_cursor2(self.output.crtc, None::<&DumbBuffer>, (0, 0));
            let _ = self.card.destroy_dumb_buffer(cursor.buffer);
        }
        for b in std::mem::take(&mut self.buffers) {
            let _ = self.card.destroy_framebuffer(b.fb);
            let _ = self.card.destroy_dumb_buffer(b.buffer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::card_number;

    #[test]
    fn cards_sort_numerically() {
        let mut cards = ["/dev/dri/card10", "/dev/dri/card1", "/dev/dri/card0", "/dev/dri/card9"];
        cards.sort_by_key(|p| card_number(p));
        assert_eq!(cards, ["/dev/dri/card0", "/dev/dri/card1", "/dev/dri/card9", "/dev/dri/card10"]);
    }
}
