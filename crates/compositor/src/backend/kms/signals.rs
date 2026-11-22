//! Stop signals as a file descriptor the frame loop polls, so the compositor exits
//! normally and its cleanup runs: the console's keyboard comes back, and the cursor
//! and scanout buffers are released. session stops services with SIGTERM.
//!
//! The signals are blocked and read through a signalfd instead of handled
//! asynchronously, so nothing runs in signal context.

use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::raw::c_int;

/// The signals that mean "shut down".
const STOP: [c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

pub struct Signals {
    fd: OwnedFd,
}

impl Signals {
    /// Block the stop signals on this thread and open a signalfd for them.
    ///
    /// Call this before any other thread starts (wgpu and Mesa start their own):
    /// new threads inherit the mask, and a stop signal delivered to a thread that
    /// doesn't block it would kill the process without cleanup.
    pub fn block() -> Result<Signals, String> {
        let mut set = MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: sigemptyset initialises the set; sigaddset only adds valid
        // signal numbers to it; both pointers are valid for each call.
        let set = unsafe {
            libc::sigemptyset(set.as_mut_ptr());
            for sig in STOP {
                libc::sigaddset(set.as_mut_ptr(), sig);
            }
            set.assume_init()
        };
        // SAFETY: `set` is initialised; a null old-set pointer is allowed.
        let err = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
        if err != 0 {
            return Err(format!("cannot block stop signals: {}", io::Error::from_raw_os_error(err)));
        }
        // SAFETY: -1 asks for a new fd; `set` is valid.
        let fd = unsafe { libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
        if fd < 0 {
            return Err(format!("signalfd: {}", io::Error::last_os_error()));
        }
        // SAFETY: signalfd returned a new fd that nothing else owns.
        Ok(Signals { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    /// Readable when a stop signal is pending.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The name of a pending stop signal, consuming it, or None if none is pending.
    pub fn take(&self) -> Option<&'static str> {
        let mut info = MaybeUninit::<libc::signalfd_siginfo>::uninit();
        let size = size_of::<libc::signalfd_siginfo>();
        // SAFETY: the buffer holds exactly one signalfd_siginfo.
        let n = unsafe { libc::read(self.fd.as_raw_fd(), info.as_mut_ptr().cast(), size) };
        if n != size as isize {
            return None;
        }
        // SAFETY: the kernel filled the whole struct.
        Some(name(unsafe { info.assume_init() }.ssi_signo as c_int))
    }
}

fn name(sig: c_int) -> &'static str {
    match sig {
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        libc::SIGHUP => "SIGHUP",
        _ => "a stop signal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_signal_is_read_from_the_fd() {
        // Runs on its own thread, so the mask doesn't leak into other tests, and
        // the signal is sent to that thread only.
        std::thread::spawn(|| {
            let signals = Signals::block().expect("signalfd");
            assert_eq!(signals.take(), None, "nothing pending yet");
            // SAFETY: signals this thread, which blocks SIGTERM, so it stays pending.
            assert_eq!(unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTERM) }, 0);
            assert_eq!(signals.take(), Some("SIGTERM"));
            assert_eq!(signals.take(), None, "consumed");
        })
        .join()
        .expect("test thread");
    }
}
