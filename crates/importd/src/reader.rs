//! Reading untrusted files outside importd: what `import-reader` does, and how
//! importd talks to it.
//!
//! importd runs as root, so it never decodes a music file or a picture itself.
//! Per import it starts `import-reader` in the tightest sandbox (see the
//! `sandbox` crate): its own Unix user, no network, no file it may open, nowhere
//! to write, limited memory. For each file importd opens the file and passes the
//! descriptor over a socket pair; the reader answers with what it found. A
//! reader that dies, hangs or answers nonsense fails that one file, and importd
//! starts another for the rest.
//!
//! On the wire, importd sends one message per file: a byte saying what to read
//! (`a` music, `i` a picture) and the file's extension, with the descriptor
//! attached. The reader answers with a little-endian `u32` length, that many
//! bytes of JSON (a [`Reply`]), then `artwork` bytes of cover art. importd
//! trusts none of it: [`check`] bounds every field.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::audio::{self, Artwork, Tags};
use crate::kinds::{HEAD_BYTES, image_size};

/// What to read from a file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ask {
    /// A music file's tags, length and cover.
    Audio,
    /// A picture's width and height.
    Image,
}

impl Ask {
    fn byte(self) -> u8 {
        match self {
            Ask::Audio => b'a',
            Ask::Image => b'i',
        }
    }
}

/// What the reader found in a file, before importd checks it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Tags>,
    /// A picture's width and height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<[u32; 2]>,
    /// How many bytes of cover art follow, and their MIME type.
    #[serde(default)]
    pub artwork: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_mime: Option<String>,
    /// Why nothing could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The longest reply header importd reads.
pub const MAX_HEADER: u32 = 64 * 1024;
/// The largest cover importd takes.
pub const MAX_ARTWORK: u32 = 16 * 1024 * 1024;
/// The longest a tag may be, in characters; longer ones are cut.
pub const MAX_TAG_CHARS: usize = 512;
/// How long the reader gets for one file.
pub const PER_FILE: Duration = Duration::from_secs(30);
/// The Unix user the reader runs as (when importd is root): one nothing else
/// uses, which owns no files and can't log in.
pub const READER_USER: u32 = 61_000;
/// How much memory the reader may map.
pub const READER_MEMORY: u64 = 256 * 1024 * 1024;

/// What a checked reply says about a file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found {
    pub tags: Tags,
    pub size: Option<(u32, u32)>,
    /// Why the reader could read nothing (the file is still imported, by its name).
    pub problem: Option<String>,
}

/// Bound everything in `reply` (and its `artwork`), which came from a process
/// that may be in a bad file's hands: tags cut to [`MAX_TAG_CHARS`], numbers kept
/// only when plausible, cover art only with a picture type.
pub fn check(reply: Reply, artwork: Vec<u8>) -> Found {
    if let Some(e) = reply.error {
        return Found {
            problem: Some(e.chars().filter(|c| !c.is_control()).take(200).collect()),
            ..Found::default()
        };
    }
    let mut tags = reply.tags.unwrap_or_default();
    for t in [&mut tags.title, &mut tags.artist, &mut tags.album, &mut tags.album_artist, &mut tags.genre] {
        *t = t.take().map(|s| s.chars().filter(|c| !c.is_control()).take(MAX_TAG_CHARS).collect());
        if t.as_deref().is_some_and(|s| s.trim().is_empty()) {
            *t = None;
        }
    }
    tags.track = tags.track.filter(|n| (1..=9999).contains(n));
    tags.disc = tags.disc.filter(|n| (1..=999).contains(n));
    tags.year = tags.year.filter(|y| (1..=9999).contains(y));
    tags.duration = tags.duration.filter(|d| d.is_finite() && *d > 0.0 && *d < 1_000_000.0);
    let picture = ["image/png", "image/jpeg", "image/gif", "image/webp", "image/bmp", "image/tiff"];
    tags.artwork = match reply.artwork_mime {
        Some(m) if !artwork.is_empty() && picture.contains(&m.as_str()) => {
            Some(Artwork { bytes: artwork, mime: Some(m) })
        }
        _ => None,
    };
    let size = reply
        .size
        .map(|[w, h]| (w, h))
        .filter(|&(w, h)| (1..=1_000_000).contains(&w) && (1..=1_000_000).contains(&h));
    Found { tags, size, problem: None }
}

/// Send `bytes` on `sock` with descriptor `fd` attached.
pub fn send_fd(sock: &UnixStream, bytes: &[u8], fd: BorrowedFd) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut _, iov_len: bytes.len() };
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(size_of::<i32>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: msghdr is plain data; the zeroed fields are filled in below.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut _;
    msg.msg_controllen = space as _;
    // SAFETY: the control buffer has room for one header and one descriptor.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<i32>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut i32, raw);
    }
    // SAFETY: sendmsg(2) with buffers that outlive the call.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != bytes.len() {
        return Err(io::Error::other("a short send"));
    }
    Ok(())
}

/// Receive a message into `buf` and the descriptor sent with it. None when the
/// other end has closed.
pub fn recv_fd(sock: &UnixStream, buf: &mut [u8]) -> io::Result<Option<(usize, OwnedFd)>> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut _, iov_len: buf.len() };
    // SAFETY: as in send_fd.
    let space = unsafe { libc::CMSG_SPACE(size_of::<i32>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: as in send_fd.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut _;
    msg.msg_controllen = space as _;
    // SAFETY: recvmsg(2) into buffers that outlive the call.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Ok(None);
    }
    // SAFETY: walking the control messages the kernel just wrote.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if !cmsg.is_null() && (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS
        {
            let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const i32);
            return Ok(Some((n as usize, OwnedFd::from_raw_fd(fd))));
        }
    }
    Err(io::Error::other("a message without a file"))
}

/// The reader's side: answer one request, `message` (what to read and the
/// file's extension), about `file`.
pub fn answer(message: &[u8], file: &mut std::fs::File) -> (Reply, Vec<u8>) {
    let ext = std::str::from_utf8(message.get(1..).unwrap_or_default()).unwrap_or_default();
    let failed = |e: String| (Reply { error: Some(e), ..Reply::default() }, Vec::new());
    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        return failed(e.to_string());
    }
    match message.first() {
        Some(b'a') => match audio::read(file, ext) {
            Ok(mut tags) => {
                let art = tags.artwork.take().filter(|a| a.bytes.len() <= MAX_ARTWORK as usize);
                let reply = Reply {
                    artwork: art.as_ref().map_or(0, |a| a.bytes.len() as u32),
                    artwork_mime: art.as_ref().and_then(|a| a.mime.clone()),
                    tags: Some(tags),
                    ..Reply::default()
                };
                (reply, art.map(|a| a.bytes).unwrap_or_default())
            }
            Err(e) => failed(e),
        },
        Some(b'i') => {
            let mut head = Vec::new();
            match file.take(HEAD_BYTES as u64).read_to_end(&mut head) {
                Ok(_) => {
                    let size = image_size(&head).map(|(w, h)| [w, h]);
                    (Reply { size, ..Reply::default() }, Vec::new())
                }
                Err(e) => failed(e.to_string()),
            }
        }
        _ => failed("nothing to read".into()),
    }
}

/// Write `reply` and its `artwork` to `out`.
pub fn write_reply(mut out: impl Write, reply: &Reply, artwork: &[u8]) -> io::Result<()> {
    let header = serde_json::to_vec(reply).map_err(io::Error::other)?;
    out.write_all(&(header.len() as u32).to_le_bytes())?;
    out.write_all(&header)?;
    out.write_all(artwork)?;
    out.flush()
}

/// Read a reply and its cover art from `input`, refusing anything too big.
pub fn read_reply(mut input: impl Read) -> io::Result<(Reply, Vec<u8>)> {
    let mut len = [0u8; 4];
    input.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_HEADER {
        return Err(io::Error::other(format!("a {len}-byte answer")));
    }
    let mut header = vec![0u8; len as usize];
    input.read_exact(&mut header)?;
    let reply: Reply = serde_json::from_slice(&header).map_err(io::Error::other)?;
    if reply.artwork > MAX_ARTWORK {
        return Err(io::Error::other(format!("a {}-byte cover", reply.artwork)));
    }
    let mut artwork = vec![0u8; reply.artwork as usize];
    input.read_exact(&mut artwork)?;
    Ok((reply, artwork))
}

/// importd's side: the reader process, started when first needed.
pub struct Reader {
    program: PathBuf,
    /// How long it gets for each file.
    timeout: Duration,
    running: Option<(Child, UnixStream)>,
}

impl Reader {
    /// A reader that will run `program` (`import-reader`).
    pub fn new(program: PathBuf) -> Reader {
        Reader { program, timeout: PER_FILE, running: None }
    }

    /// The same, giving it `timeout` for each file instead of [`PER_FILE`].
    pub fn with_timeout(mut self, timeout: Duration) -> Reader {
        self.timeout = timeout;
        self
    }

    /// Read `ask` from `file` (whose extension is `ext`), starting the reader if
    /// it isn't running. An error means the reader itself failed (died, hung or
    /// answered nonsense): it's stopped, so the next file gets a fresh one.
    pub fn read(&mut self, ask: Ask, file: &std::fs::File, ext: &str) -> Result<Found, String> {
        if self.running.is_none() {
            self.running = Some(self.start().map_err(|e| format!("cannot start import-reader: {e}"))?);
        }
        let (_, sock) = self.running.as_ref().expect("started");
        let mut message = vec![ask.byte()];
        message.extend(ext.bytes().filter(u8::is_ascii_alphanumeric).take(16));
        use std::os::fd::AsFd;
        let answer = send_fd(sock, &message, file.as_fd()).and_then(|()| read_reply(sock));
        match answer {
            Ok((reply, artwork)) => Ok(check(reply, artwork)),
            Err(e) => {
                self.stop();
                let e = if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) {
                    format!("reading it took over {:.1} s", self.timeout.as_secs_f64())
                } else {
                    format!("reading it failed ({e})")
                };
                Err(e)
            }
        }
    }

    /// Stop the reader, if it's running (at the end of an import, or after it failed).
    pub fn stop(&mut self) {
        if let Some((mut child, sock)) = self.running.take() {
            drop(sock);
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn start(&self) -> io::Result<(Child, UnixStream)> {
        let (ours, theirs) = UnixStream::pair()?;
        ours.set_read_timeout(Some(self.timeout))?;
        ours.set_write_timeout(Some(self.timeout))?;
        let mut cmd = Command::new(&self.program);
        cmd.stdin(Stdio::from(OwnedFd::from(theirs))).stdout(Stdio::null()).current_dir("/");
        if std::env::var_os("IDEALOS_NO_SANDBOX").is_none() {
            use sandbox::{Limit, Sandbox, Spec};
            let limits = vec![
                (Limit::Memory, READER_MEMORY),
                (Limit::FileSize, 0),
                (Limit::OpenFiles, 16),
                (Limit::CoreSize, 0),
            ];
            let sb = Sandbox::prepare(&Spec::for_helper(&self.program, READER_USER, limits))?;
            if cfg!(target_os = "linux") && !sb.limits_files() {
                log::warn!("importd: this kernel has no Landlock, so import-reader has no file limits");
            }
            use std::os::unix::process::CommandExt;
            // SAFETY: enter only makes system calls, between fork and exec.
            unsafe { cmd.pre_exec(move || sb.enter()) };
        } else {
            log::warn!("importd: starting import-reader without a sandbox (IDEALOS_NO_SANDBOX)");
        }
        let child = cmd.spawn()?;
        Ok((child, ours))
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_go_through_the_wire_and_are_bounded() {
        let tags = Tags {
            title: Some(format!("{}\u{7}", "x".repeat(2000))),
            artist: Some("  ".into()),
            track: Some(70_000),
            year: Some(1991),
            duration: Some(-5.0),
            ..Tags::default()
        };
        let reply = Reply {
            tags: Some(tags),
            artwork: 3,
            artwork_mime: Some("image/png".into()),
            ..Reply::default()
        };
        let mut wire = Vec::new();
        write_reply(&mut wire, &reply, b"png").unwrap();
        let (back, art) = read_reply(wire.as_slice()).unwrap();
        assert_eq!(back, reply);
        let found = check(back, art);
        assert_eq!(found.tags.title.as_ref().unwrap().chars().count(), MAX_TAG_CHARS);
        assert!(!found.tags.title.unwrap().contains('\u{7}'));
        assert_eq!((found.tags.artist, found.tags.track, found.tags.year), (None, None, Some(1991)));
        assert_eq!(found.tags.duration, None);
        assert_eq!(found.tags.artwork.unwrap().bytes, b"png");

        // Cover art that isn't a picture is dropped; an error is the error.
        let odd = Reply { artwork: 1, artwork_mime: Some("text/html".into()), ..Reply::default() };
        assert_eq!(check(odd, b"<".to_vec()).tags.artwork, None);
        let bad = Reply { error: Some("no tags".into()), ..Reply::default() };
        assert_eq!(check(bad, Vec::new()).problem.as_deref(), Some("no tags"));
    }

    #[test]
    fn oversized_or_unknown_replies_are_refused() {
        let mut wire = (MAX_HEADER + 1).to_le_bytes().to_vec();
        wire.resize(wire.len() + 8, b' ');
        assert!(read_reply(wire.as_slice()).is_err());
        let big = format!("{{\"artwork\":{}}}", MAX_ARTWORK + 1);
        let mut wire = (big.len() as u32).to_le_bytes().to_vec();
        wire.extend(big.as_bytes());
        assert!(read_reply(wire.as_slice()).is_err());
        let unknown = b"{\"root\":true}";
        let mut wire = (unknown.len() as u32).to_le_bytes().to_vec();
        wire.extend(unknown);
        assert!(read_reply(wire.as_slice()).is_err());
    }

    /// A script standing in for the reader, at a new path under the temp directory.
    fn script(name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("reader-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reader");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn a_reader_that_hangs_or_dies_fails_the_file_and_is_replaced() {
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let hang = script("hang", "sleep 5");
        let mut r = Reader::new(hang.clone()).with_timeout(Duration::from_millis(300));
        let started = std::time::Instant::now();
        let e = r.read(Ask::Audio, &file, "mp3").unwrap_err();
        assert!(e.contains("took over 0.3 s"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(3), "it didn't wait for the reader");
        assert!(r.running.is_none(), "the hung reader was stopped");

        let die = script("die", "exit 1");
        let mut r = Reader::new(die.clone());
        let e = r.read(Ask::Image, &file, "png").unwrap_err();
        assert!(e.contains("reading it failed"), "{e}");
        assert!(r.running.is_none());
        let e = r.read(Ask::Image, &file, "png").unwrap_err();
        assert!(e.contains("reading it failed"), "a fresh one is started, and fails the same: {e}");
        for p in [hang, die] {
            std::fs::remove_dir_all(p.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn a_file_goes_across_with_its_message() {
        let (a, b) = UnixStream::pair().unwrap();
        let path = std::env::temp_dir().join(format!("reader-fd-{}", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        use std::os::fd::AsFd;
        send_fd(&a, b"imd", file.as_fd()).unwrap();
        let mut buf = [0u8; 32];
        let (n, fd) = recv_fd(&b, &mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"imd");
        let mut got = String::new();
        std::fs::File::from(fd).read_to_string(&mut got).unwrap();
        assert_eq!(got, "hello");
        drop(a);
        assert!(recv_fd(&b, &mut buf).unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
    }
}
