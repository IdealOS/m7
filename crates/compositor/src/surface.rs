//! The `surface` node: frames another process draws (a web page), in a window.
//!
//! An app creates a `surface` node naming a service and a session of its own
//! there (`service: "web"`, `id`). The compositor attaches to the session itself,
//! as the app ([`protocol::surface`]): the service sends shared-memory buffers,
//! then a part for each frame it draws into one. Each frame is copied out here
//! into an [`Image`], drawn like an image node's, and the buffer is handed back.
//! The node's size, in screen pixels, is sent as the surface's size, and the
//! pointer and keys go to the service in those pixels.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use protocol::surface::{MAX_BUFFERS, MAX_SIDE, SurfacePart};
use serde_json::{Value, json};

use crate::image::Image;
use crate::input::{Key, Mods};

/// One surface node's session and its latest frame.
#[derive(Debug)]
pub struct View {
    pub service: String,
    pub session: String,
    /// Who it's shown for, which the compositor acts as: an app's id, or `conn:N`
    /// for an app run as a tool in development (see `Apps::owner_of`).
    pub app: Option<String>,
    buffers: Vec<File>,
    width: u32,
    height: u32,
    stride: u32,
    /// The size last asked of the service, in pixels.
    pub asked: Option<(u32, u32)>,
    /// The image-cache key of the frame shown now (a new key per frame, so the
    /// renderer and damage tracking see each frame as new pixels).
    pub key: Option<String>,
}

/// What a part from the service amounts to.
#[derive(Debug)]
pub enum Got {
    /// New buffers; nothing to draw yet.
    Buffers,
    /// A frame, copied out: show it, then hand buffer `buffer` back.
    Frame { image: Arc<Image>, buffer: u32, seq: u64 },
}

impl View {
    pub fn new(service: String, session: String, app: Option<String>) -> View {
        View {
            service,
            session,
            app,
            buffers: Vec::new(),
            width: 0,
            height: 0,
            stride: 0,
            asked: None,
            key: None,
        }
    }

    /// Params for a request about this session, made as its app.
    pub fn params(&self, mut extra: Value) -> Value {
        extra["id"] = json!(self.session);
        if let Some(app) = &self.app {
            extra["as"] = json!(app);
        }
        extra
    }

    /// Take a part of the attachment, and the descriptors that came with it.
    pub fn take(&mut self, part: Value, fds: Vec<OwnedFd>) -> Result<Got, String> {
        match serde_json::from_value::<SurfacePart>(part).map_err(|e| format!("a surface part: {e}"))? {
            SurfacePart::Buffers { width, height, stride, count } => {
                if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
                    return Err(format!("a {width}x{height} surface"));
                }
                if stride < width * 4 || stride > MAX_SIDE * 4 {
                    return Err(format!("stride {stride} for {width} pixels"));
                }
                if count == 0 || count > MAX_BUFFERS || fds.len() != count as usize {
                    return Err(format!("{} descriptors for {count} buffers", fds.len()));
                }
                let need = u64::from(stride) * u64::from(height);
                let mut buffers = Vec::with_capacity(fds.len());
                for fd in fds {
                    let file = File::from(fd);
                    // A buffer shorter than it says would make every read fail.
                    let len = file.metadata().map_err(|e| e.to_string())?.len();
                    if len < need {
                        return Err(format!("a {len}-byte buffer for {need} bytes"));
                    }
                    buffers.push(file);
                }
                (self.buffers, self.width, self.height, self.stride) = (buffers, width, height, stride);
                Ok(Got::Buffers)
            }
            SurfacePart::Frame { buffer, seq } => {
                let file = self.buffers.get(buffer as usize).ok_or(format!("a frame in buffer {buffer}"))?;
                Ok(Got::Frame { image: Arc::new(self.read(file)?), buffer, seq })
            }
        }
    }

    /// The pixels in `file`, rows packed.
    fn read(&self, file: &File) -> Result<Image, String> {
        let row = self.width as usize * 4;
        let mut rgba = vec![0u8; row * self.height as usize];
        if self.stride as usize == row {
            file.read_exact_at(&mut rgba, 0).map_err(|e| e.to_string())?;
        } else {
            for (y, out) in rgba.chunks_exact_mut(row).enumerate() {
                file.read_exact_at(out, y as u64 * u64::from(self.stride)).map_err(|e| e.to_string())?;
            }
        }
        Ok(Image { width: self.width, height: self.height, rgba })
    }
}

/// A node `w` by `h` (logical pixels) on a screen of `scale` pixels per logical
/// one, as a surface size: whole pixels, at least 1, at most [`MAX_SIDE`].
pub fn pixels(w: f32, h: f32, scale: f32) -> (u32, u32) {
    let px = |v: f32| ((v * scale).round() as u32).clamp(1, MAX_SIDE);
    (px(w), px(h))
}

/// A key pressed, as the web names it (`KeyboardEvent.key`).
pub fn key_name(key: Key) -> String {
    match key {
        Key::Tab => "Tab",
        Key::Enter => "Enter",
        Key::Escape => "Escape",
        Key::Backspace => "Backspace",
        Key::Delete => "Delete",
        Key::Left => "ArrowLeft",
        Key::Right => "ArrowRight",
        Key::Up => "ArrowUp",
        Key::Down => "ArrowDown",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::Char(c) => return c.to_string(),
    }
    .into()
}

pub fn mods(m: Mods) -> protocol::surface::Mods {
    protocol::surface::Mods { shift: m.shift, ctrl: m.ctrl, alt: m.alt, logo: m.logo }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// An unnamed file holding `bytes`, as a service's shared buffer would.
    fn buffer(bytes: &[u8]) -> OwnedFd {
        let path = std::env::temp_dir().join(format!(
            "surface-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        f.write_all(bytes).unwrap();
        f.into()
    }

    #[test]
    fn frames_are_copied_out_of_the_buffer_they_name() {
        let mut v = View::new("web".into(), "p1".into(), Some("org.idealos.web".into()));
        // 2x2 pixels in rows of 12 bytes (4 bytes of padding each).
        let a: Vec<u8> = (0..24).collect();
        let b = vec![9u8; 24];
        let part = json!({"part": "buffers", "width": 2, "height": 2, "stride": 12, "count": 2});
        assert!(matches!(v.take(part, vec![buffer(&a), buffer(&b)]), Ok(Got::Buffers)));
        let Ok(Got::Frame { image, buffer, seq }) =
            v.take(json!({"part": "frame", "buffer": 0, "seq": 7}), vec![])
        else {
            panic!()
        };
        assert_eq!((buffer, seq, image.width, image.height), (0, 7, 2, 2));
        assert_eq!(image.rgba, [0, 1, 2, 3, 4, 5, 6, 7, 12, 13, 14, 15, 16, 17, 18, 19]);
        let Ok(Got::Frame { image, .. }) = v.take(json!({"part": "frame", "buffer": 1, "seq": 8}), vec![])
        else {
            panic!()
        };
        assert!(image.rgba.iter().all(|&p| p == 9));
        assert_eq!(v.params(json!({"buffer": 1})), json!({"id": "p1", "as": "org.idealos.web", "buffer": 1}));
    }

    #[test]
    fn a_service_cant_make_the_compositor_read_past_what_it_sent() {
        let mut v = View::new("web".into(), "p1".into(), None);
        let part = |w: u32, h: u32, stride: u32, count: u32| json!({"part": "buffers", "width": w, "height": h, "stride": stride, "count": count});
        assert!(v.take(part(2, 2, 8, 1), vec![buffer(&[0; 15])]).is_err(), "too short");
        assert!(v.take(part(2, 2, 4, 1), vec![buffer(&[0; 16])]).is_err(), "stride under a row");
        assert!(v.take(part(MAX_SIDE + 1, 1, (MAX_SIDE + 1) * 4, 1), vec![buffer(&[0; 4])]).is_err());
        assert!(v.take(part(1, 1, 4, 2), vec![buffer(&[0; 4])]).is_err(), "fewer descriptors than buffers");
        assert!(v.take(part(1, 1, 4, 4), (0..4).map(|_| buffer(&[0; 4])).collect()).is_err(), "too many");
        assert!(v.take(json!({"part": "frame", "buffer": 0, "seq": 1}), vec![]).is_err(), "no buffers yet");
        assert!(matches!(v.take(part(1, 1, 4, 1), vec![buffer(&[1, 2, 3, 4])]), Ok(Got::Buffers)));
        assert!(v.take(json!({"part": "frame", "buffer": 1, "seq": 1}), vec![]).is_err(), "no such buffer");
    }

    #[test]
    fn sizes_and_keys_are_given_as_the_service_wants_them() {
        assert_eq!(pixels(640.5, 400.0, 2.0), (1281, 800));
        assert_eq!(pixels(0.0, 100_000.0, 1.0), (1, MAX_SIDE));
        assert_eq!(key_name(Key::Left), "ArrowLeft");
        assert_eq!(key_name(Key::Char('A')), "A");
    }
}
