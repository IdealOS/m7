//! One page's surface: the shared buffers its frames go out in, and which of
//! them the compositor still holds.

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

use protocol::surface::{MAX_SIDE, SurfacePart};
use serde_json::Value;

use crate::shm;

/// Two: one the compositor is copying out, one to draw the next frame into.
pub const BUFFERS: usize = 2;

pub struct Surface {
    /// The compositor's `attach` request, whose stream the parts go out on.
    pub req: u64,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    buffers: Vec<File>,
    /// The frame each buffer holds until the compositor releases it (None: free).
    holds: Vec<Option<u64>>,
    seq: u64,
    /// A frame should go out as soon as a buffer is free.
    pub want_frame: bool,
}

/// A size the compositor asked for, checked.
pub fn checked(width: u32, height: u32, scale: f32) -> Result<(u32, u32, f32), String> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(format!("a {width}x{height} surface"));
    }
    if !(0.25..=8.0).contains(&scale) {
        return Err(format!("scale {scale}"));
    }
    Ok((width, height, scale))
}

impl Surface {
    /// A surface for attachment `req`, with no buffers until [`Surface::resize`].
    pub fn new(req: u64) -> Surface {
        Surface {
            req,
            width: 0,
            height: 0,
            scale: 1.0,
            buffers: Vec::new(),
            holds: Vec::new(),
            seq: 0,
            want_frame: true,
        }
    }

    /// New buffers for a `width` by `height` surface: the part announcing them,
    /// and the descriptors to send with it.
    pub fn resize(&mut self, width: u32, height: u32, scale: f32) -> io::Result<(Value, Vec<OwnedFd>)> {
        let stride = width * 4;
        let buffers: Vec<File> = (0..BUFFERS)
            .map(|_| shm::buffer(u64::from(stride) * u64::from(height)))
            .collect::<io::Result<_>>()?;
        let fds = buffers.iter().map(|b| b.try_clone().map(OwnedFd::from)).collect::<io::Result<_>>()?;
        (self.width, self.height, self.scale) = (width, height, scale);
        self.buffers = buffers;
        self.holds = vec![None; BUFFERS];
        self.want_frame = true;
        let part = SurfacePart::Buffers { width, height, stride, count: BUFFERS as u32 };
        Ok((serde_json::to_value(part).expect("serializes"), fds))
    }

    /// The compositor has copied frame `seq` out of buffer `buffer`.
    pub fn release(&mut self, buffer: u32, seq: u64) {
        if let Some(h) = self.holds.get_mut(buffer as usize)
            && *h == Some(seq)
        {
            *h = None;
        }
    }

    pub fn has_free_buffer(&self) -> bool {
        self.holds.iter().any(Option::is_none)
    }

    /// Put a `width` by `height` RGBA frame into a free buffer, returning the part
    /// that says so (None if every buffer is held). A frame of another size (drawn
    /// before a resize caught up) fills what overlaps.
    pub fn put(&mut self, width: u32, height: u32, rgba: &[u8]) -> io::Result<Option<Value>> {
        let Some(i) = self.holds.iter().position(Option::is_none) else { return Ok(None) };
        let file = &self.buffers[i];
        let stride = self.width as usize * 4;
        if width == self.width && height == self.height {
            file.write_all_at(&rgba[..stride * height as usize], 0)?;
        } else {
            let row = width.min(self.width) as usize * 4;
            for y in 0..height.min(self.height) as usize {
                file.write_all_at(&rgba[y * width as usize * 4..][..row], (y * stride) as u64)?;
            }
        }
        self.seq += 1;
        self.holds[i] = Some(self.seq);
        self.want_frame = false;
        let part = SurfacePart::Frame { buffer: i as u32, seq: self.seq };
        Ok(Some(serde_json::to_value(part).expect("serializes")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(fd: &OwnedFd, len: usize) -> Vec<u8> {
        let f = File::from(fd.try_clone().unwrap());
        let mut out = vec![0; len];
        f.read_exact_at(&mut out, 0).unwrap();
        out
    }

    #[test]
    fn frames_go_into_free_buffers_until_released() {
        let mut s = Surface::new(9);
        let (part, fds) = s.resize(2, 1, 2.0).unwrap();
        assert_eq!(part, json!({"part": "buffers", "width": 2, "height": 1, "stride": 8, "count": 2}));
        assert_eq!(fds.len(), BUFFERS);
        assert_eq!(s.put(2, 1, &[1; 8]).unwrap(), Some(json!({"part": "frame", "buffer": 0, "seq": 1})));
        assert_eq!(s.put(2, 1, &[2; 8]).unwrap(), Some(json!({"part": "frame", "buffer": 1, "seq": 2})));
        assert_eq!(read(&fds[0], 8), [1; 8]);
        assert_eq!(read(&fds[1], 8), [2; 8]);
        assert!(!s.has_free_buffer());
        assert_eq!(s.put(2, 1, &[3; 8]).unwrap(), None, "both held");
        // A release naming an older frame in that buffer frees nothing.
        s.release(0, 7);
        assert!(!s.has_free_buffer());
        s.release(0, 1);
        assert_eq!(s.put(2, 1, &[3; 8]).unwrap(), Some(json!({"part": "frame", "buffer": 0, "seq": 3})));
        assert_eq!(read(&fds[0], 8), [3; 8]);
    }

    #[test]
    fn a_frame_of_the_old_size_fills_what_overlaps() {
        let mut s = Surface::new(1);
        let (_, fds) = s.resize(2, 2, 1.0).unwrap();
        // 1x1, then 3x3: neither spills over the 2x2 buffer.
        s.put(1, 1, &[5; 4]).unwrap();
        assert_eq!(read(&fds[0], 16), [[5; 4], [0; 4], [0; 4], [0; 4]].concat());
        let big: Vec<u8> = (0..36).collect();
        s.put(3, 3, &big).unwrap();
        assert_eq!(read(&fds[1], 16), [&big[0..8], &big[12..20]].concat());
        assert!(checked(0, 5, 1.0).is_err() && checked(5, 5, 100.0).is_err() && checked(5, 5, 2.0).is_ok());
    }
}
