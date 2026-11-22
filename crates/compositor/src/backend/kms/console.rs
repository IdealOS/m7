//! Keeps the kernel's text console out of the way while the compositor owns the display.
//!
//! The kernel's keyboard driver hands every key to the foreground virtual terminal
//! as well as to evdev. On the Pi that terminal is `/dev/console` (`console=tty1`),
//! where session runs a root shell, so without this every key typed into the desktop
//! would also reach that shell. Turning the terminal's keyboard off (`K_OFF`) stops
//! that, and graphics mode (`KD_GRAPHICS`) stops the console drawing over the
//! display. Both are put back when the compositor exits, including on SIGTERM (see
//! [`super::signals`]). After a SIGKILL they stay off until the next compositor exits
//! cleanly, or `kbd_mode -u -C /dev/tty1` by hand.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::os::unix::fs::OpenOptionsExt;

// From linux/kd.h.
const KDSETMODE: libc::Ioctl = 0x4B3A;
const KD_TEXT: c_int = 0x00;
const KD_GRAPHICS: c_int = 0x01;
const KDGKBMODE: libc::Ioctl = 0x4B44;
const KDSKBMODE: libc::Ioctl = 0x4B45;
const K_UNICODE: c_int = 0x03;
const K_OFF: c_int = 0x04;

/// The foreground virtual terminal, whichever it is.
pub const FOREGROUND: &str = "/dev/tty0";

/// The muted terminal; dropping it restores the keyboard and text mode.
pub struct Console {
    tty: File,
    keyboard: c_int,
}

impl Console {
    /// Turn off the keyboard and text drawing of the terminal at `path`.
    pub fn mute(path: &str) -> Result<Console, String> {
        // O_NOCTTY: never make this terminal the compositor's controlling terminal.
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)
            .map_err(|e| format!("{path}: {e}"))?;
        let fd = tty.as_raw_fd();
        let mut saved: c_int = 0;
        // SAFETY: KDGKBMODE writes one int to the pointer, which is valid for the call.
        check(unsafe { libc::ioctl(fd, KDGKBMODE, &mut saved) }, path, "read the keyboard mode")?;
        // The values are passed directly, not by pointer.
        check(unsafe { libc::ioctl(fd, KDSKBMODE, K_OFF) }, path, "turn the keyboard off")?;
        let console = Console { tty, keyboard: restore_mode(saved) };
        check(unsafe { libc::ioctl(fd, KDSETMODE, KD_GRAPHICS) }, path, "switch to graphics mode")?;
        Ok(console)
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        let fd = self.tty.as_raw_fd();
        // SAFETY: plain-value ioctls on an fd this struct owns. Errors are ignored:
        // there is nothing better to do while shutting down.
        unsafe {
            libc::ioctl(fd, KDSETMODE, KD_TEXT);
            libc::ioctl(fd, KDSKBMODE, self.keyboard);
        }
    }
}

/// The keyboard mode to put back. If the saved mode is already off, a previous
/// compositor died without restoring it, so fall back to the normal Unicode mode
/// rather than leave the console deaf for good.
fn restore_mode(saved: c_int) -> c_int {
    if saved == K_OFF { K_UNICODE } else { saved }
}

fn check(ret: c_int, path: &str, what: &str) -> Result<(), String> {
    if ret < 0 { Err(format!("{path}: cannot {what}: {}", io::Error::last_os_error())) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_console_left_off_is_restored_to_unicode() {
        assert_eq!(restore_mode(K_OFF), K_UNICODE);
        // K_XLATE (1) and K_UNICODE come back as they were.
        assert_eq!(restore_mode(0x01), 0x01);
        assert_eq!(restore_mode(K_UNICODE), K_UNICODE);
    }
}
