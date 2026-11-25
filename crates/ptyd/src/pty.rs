//! A shell on a pseudo-terminal.
//!
//! [`spawn`] opens a pty pair, starts the shell as a new session with the pty's
//! slave side as its controlling terminal and its standard streams, and keeps the
//! master side, non-blocking, for reading the shell's output and writing its input.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use tokio::io::unix::AsyncFd;
use tokio::process::{Child, Command};

pub struct Pty {
    pub master: AsyncFd<OwnedFd>,
    pub child: Child,
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

/// Start `argv` on a new `cols` by `rows` pty, in `dir`.
pub fn spawn(argv: &[String], dir: &std::path::Path, cols: u16, rows: u16) -> io::Result<Pty> {
    let (program, args) = argv.split_first().ok_or_else(|| io::Error::other("no shell to run"))?;
    let (mut master, mut slave): (RawFd, RawFd) = (-1, -1);
    let mut size = winsize(cols, rows);
    // SAFETY: openpty fills in two descriptors, owned right after.
    let r = unsafe {
        // A raw pointer: Linux takes the size as const, macOS as mutable.
        libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &raw mut size)
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: descriptors openpty just made, owned by nothing else.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        // SAFETY: fcntl on descriptors we own.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let stdio = || slave.try_clone().map(std::process::Stdio::from);
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env_remove("IDEALOS_BUS_FD")
        .stdin(stdio()?)
        .stdout(stdio()?)
        .stderr(stdio()?)
        .kill_on_drop(true);
    // SAFETY: setsid and ioctl are async-signal-safe; this runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // The slave (now standard input) becomes the controlling terminal.
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(slave);
    // SAFETY: fcntl on the master, which we own.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    // SAFETY: an OwnedFd keeps its one descriptor open until it's dropped, which
    // is when the AsyncFd is.
    let master = unsafe { AsyncFd::register(master) }.map_err(|e| io::Error::other(e.to_string()))?;
    Ok(Pty { master, child })
}

impl Pty {
    /// Tell the shell its terminal is now `cols` by `rows` (it gets SIGWINCH).
    pub fn resize(&self, cols: u16, rows: u16) {
        let size = winsize(cols, rows);
        // SAFETY: TIOCSWINSZ with a winsize struct.
        unsafe { libc::ioctl(self.master.get_ref().as_raw_fd(), libc::TIOCSWINSZ as _, &size) };
    }

    /// Read what the shell wrote, into `buf`. `Ok(0)` once the shell is gone.
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.master.readable().await?;
            match guard.try_io(|fd| {
                // SAFETY: read(2) into a buffer of the given length.
                let n = unsafe { libc::read(fd.get_ref().as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
            }) {
                Ok(Ok(n)) => return Ok(n),
                // Linux reports the slave side closing (the shell exited) as EIO.
                Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    /// Write all of `bytes` to the shell.
    pub async fn write(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let mut guard = self.master.writable().await?;
            match guard.try_io(|fd| {
                // SAFETY: write(2) from a buffer of the given length.
                let n = unsafe { libc::write(fd.get_ref().as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
                if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
            }) {
                Ok(Ok(n)) => bytes = &bytes[n..],
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// Hang up: SIGHUP to the shell's whole process group, as closing a terminal does.
    pub fn hangup(&self) {
        if let Some(pid) = self.child.id() {
            // SAFETY: kill(2) on the process group the shell leads.
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGHUP) };
        }
    }
}
