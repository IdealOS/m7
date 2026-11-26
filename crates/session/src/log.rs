//! The system log: everything session and its services print, kept in a file as
//! well as on the console, so it can be read over SSH (`tools/pi-state`).
//!
//! With `log PATH` in the config, session points its own stdout and stderr at a
//! pipe, which every service then inherits, except those marked `console` (the
//! console shell needs the terminal itself). A thread copies what arrives to the
//! console unchanged and to the file with each line stamped with the time since boot,
//! as `dmesg` stamps the kernel's, so the two read side by side (the Pi's clock
//! starts at 1970 until something sets it). The file is capped at [`MAX_BYTES`]: it
//! then becomes `PATH.1`, replacing the previous one.
//!
//! The thread never panics and never stops reading: a full pipe would block every
//! service that prints. If the file can't be written, it carries on with the
//! console alone.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

/// Size at which the log moves to `PATH.1`. Two files of this size bound the log's
/// space on `/data`.
pub const MAX_BYTES: u64 = 1024 * 1024;

pub struct Log {
    /// The console, for services marked `console`.
    console: OwnedFd,
    thread: JoinHandle<()>,
}

impl Log {
    /// Send session's stdout and stderr (and so every service's) through the logger.
    /// On an error nothing has changed and output goes to the console as before.
    pub fn start(path: &Path) -> io::Result<Log> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = LogFile::open(path.to_owned())?;
        let console = dup_cloexec(libc::STDERR_FILENO)?;
        let console_writer = File::from(dup_cloexec(console.as_raw_fd())?);
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe(2) returns.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe(2) just returned these and nothing else owns them.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        set_cloexec(&read)?;
        let reader = File::from(read);
        let thread = std::thread::Builder::new()
            .name("log".into())
            .spawn(move || copy(reader, console_writer, file))?;
        // dup2 leaves the new 1 and 2 inheritable, so services get them; the
        // original write end closes when `write` drops.
        for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            // SAFETY: both descriptors are open; dup2 only replaces `fd`.
            if unsafe { libc::dup2(write.as_raw_fd(), fd) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Log { console, thread })
    }

    /// A copy of the console, for a service's stdout or stderr.
    pub fn console(&self) -> io::Result<OwnedFd> {
        self.console.try_clone()
    }

    /// Write out what's left, once every service has exited: closing session's
    /// own ends of the pipe ends the copy. Gives up after a second, in case a
    /// service's child still holds the pipe open.
    pub fn finish(self) {
        let _ = io::stdout().flush();
        for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            // SAFETY: point 1 and 2 at the console instead of the pipe, so anything
            // printed after this still shows up.
            unsafe { libc::dup2(self.console.as_raw_fd(), fd) };
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !self.thread.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Copy the pipe to the console and the file until every writer has closed it.
fn copy(mut pipe: File, mut console: File, mut file: LogFile) {
    let mut buf = [0u8; 8192];
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let _ = console.write_all(&buf[..n]);
        file.write(&buf[..n], since_boot());
    }
}

/// The log file and its rotation. Stops writing (and says so on the console, once)
/// if a write fails, rather than retrying on a full or failing card.
struct LogFile {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    stamper: Stamper,
}

impl LogFile {
    fn open(path: PathBuf) -> io::Result<LogFile> {
        let file = open_log(&path)?;
        let size = file.metadata()?.len();
        let mut log = LogFile { path, file: Some(file), size, stamper: Stamper::default() };
        log.write(b"--- session started\n", since_boot());
        Ok(log)
    }

    fn write(&mut self, bytes: &[u8], now: Duration) {
        let stamped = self.stamper.stamp(bytes, now);
        let Some(file) = &mut self.file else { return };
        if let Err(e) = file.write_all(&stamped) {
            self.file = None;
            let line = format!(
                "session: log: can't write {}: {e}; the console only from now on\n",
                self.path.display()
            );
            // Not to stderr: that's the pipe this thread is reading.
            if let Ok(mut c) = OpenOptions::new().write(true).open("/dev/console") {
                let _ = c.write_all(line.as_bytes());
            }
            return;
        }
        self.size += stamped.len() as u64;
        if self.size >= MAX_BYTES && self.stamper.at_line_start {
            self.rotate();
        }
    }

    /// Move the full log to `PATH.1` and start a new one.
    fn rotate(&mut self) {
        let old = rotated(&self.path);
        self.file = None;
        let _ = std::fs::rename(&self.path, &old);
        self.file = open_log(&self.path).ok();
        self.size = 0;
    }
}

/// Open for appending, creating it readable by root only: services may print
/// anything.
fn open_log(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).mode(0o600).open(path)
}

/// `PATH.1`.
fn rotated(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// Puts `[  seconds.micros] ` before each line, as `dmesg` does. Remembers whether
/// the last chunk ended mid-line, since a pipe read can split a line anywhere.
struct Stamper {
    at_line_start: bool,
}

impl Default for Stamper {
    fn default() -> Self {
        Stamper { at_line_start: true }
    }
}

impl Stamper {
    fn stamp(&mut self, bytes: &[u8], now: Duration) -> Vec<u8> {
        let stamp = format!("[{:5}.{:06}] ", now.as_secs(), now.subsec_micros());
        let mut out = Vec::with_capacity(bytes.len() + stamp.len());
        for &b in bytes {
            if self.at_line_start {
                out.extend_from_slice(stamp.as_bytes());
                self.at_line_start = false;
            }
            out.push(b);
            if b == b'\n' {
                self.at_line_start = true;
            }
        }
        out
    }
}

/// Time since boot, counting suspend, as the kernel log's stamps do.
fn since_boot() -> Duration {
    #[cfg(target_os = "linux")]
    let clock = libc::CLOCK_BOOTTIME;
    #[cfg(not(target_os = "linux"))]
    let clock = libc::CLOCK_MONOTONIC;
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid timespec for clock_gettime to fill in.
    unsafe { libc::clock_gettime(clock, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn dup_cloexec(fd: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC makes a new descriptor, which we then own.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if new == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl just returned it.
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

fn set_cloexec(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: plain fcntl on a descriptor we own.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: f64) -> Duration {
        Duration::from_secs_f64(secs)
    }

    #[test]
    fn stamps_each_line_like_dmesg() {
        let mut s = Stamper::default();
        let out = s.stamp(b"one\ntwo\n", at(12.5));
        assert_eq!(String::from_utf8(out).unwrap(), "[   12.500000] one\n[   12.500000] two\n");
    }

    #[test]
    fn a_line_split_across_reads_is_stamped_once() {
        let mut s = Stamper::default();
        let mut out = s.stamp(b"sta", at(1.0));
        out.extend(s.stamp(b"rted\nnext", at(2.0)));
        out.extend(s.stamp(b" line\n", at(3.0)));
        assert_eq!(String::from_utf8(out).unwrap(), "[    1.000000] started\n[    2.000000] next line\n");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("session-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rotates_to_dot_one_at_a_line_end() {
        let dir = temp_dir("rotate");
        let path = dir.join("session.log");
        let mut log = LogFile::open(path.clone()).unwrap();
        let line = vec![b'x'; 1000];
        // Each line crosses MAX_BYTES in its middle (the 1000 bytes), then ends with
        // the newline: the whole line must land in the old file.
        let mut lines = 0;
        while !rotated(&path).exists() {
            log.write(&line, at(5.0));
            log.write(b"\n", at(5.0));
            lines += 1;
            assert!(lines < 2000, "never rotated");
        }
        log.write(b"after\n", at(6.0));
        let old = std::fs::read_to_string(rotated(&path)).unwrap();
        let new = std::fs::read_to_string(&path).unwrap();
        assert!(old.starts_with("[") && old.contains("--- session started"));
        assert!(old.ends_with('\n'), "a line isn't split between the files");
        assert!(old.len() as u64 >= MAX_BYTES);
        assert_eq!(new, "[    6.000000] after\n");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "only root reads the log");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn appends_to_an_existing_log() {
        let dir = temp_dir("append");
        let path = dir.join("session.log");
        std::fs::write(&path, "earlier boot\n").unwrap();
        let mut log = LogFile::open(path.clone()).unwrap();
        log.write(b"hello\n", at(1.0));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("earlier boot\n"));
        assert!(text.contains("--- session started\n"));
        assert!(text.ends_with("[    1.000000] hello\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
