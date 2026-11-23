//! Sending file descriptors with bus frames (`SCM_RIGHTS`).
//!
//! A message that declares `"fds": n` carries n descriptors as ancillary data. On a
//! stream socket the descriptors arrive with the first byte of the `sendmsg` that
//! sent them, which is never after the start of the frame that declares them. So a
//! reader keeps a queue: every descriptor received goes on the back, and each frame,
//! once fully read, takes its n from the front.
//!
//! These are plain blocking-or-nonblocking calls on a raw socket; async code calls
//! them inside its runtime's readiness checks.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

/// Most descriptors one message may carry.
pub const MAX_FDS_PER_MESSAGE: usize = 16;
/// Most descriptors one `sendmsg` carries; senders split batches to stay under it,
/// and receivers make room for this many per `recvmsg`.
pub const MAX_FDS_PER_SEND: usize = 64;

const FD_SIZE: usize = std::mem::size_of::<RawFd>();

fn cmsg_space(fds: usize) -> usize {
    // SAFETY: CMSG_SPACE is pure arithmetic.
    unsafe { libc::CMSG_SPACE((fds * FD_SIZE) as u32) as usize }
}

/// Send `bytes` with `fds` attached; returns how many bytes were sent, which may be
/// fewer than all of them. The descriptors go with whatever part was sent, so send
/// the rest without them.
pub fn send_with_fds(sock: BorrowedFd, bytes: &[u8], fds: &[BorrowedFd]) -> io::Result<usize> {
    assert!(fds.len() <= MAX_FDS_PER_SEND, "split batches of descriptors");
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut _, iov_len: bytes.len() };
    // u64 elements keep the control buffer aligned for cmsghdr.
    let mut control = vec![0u64; cmsg_space(MAX_FDS_PER_SEND).div_ceil(8)];
    // SAFETY: an all-zero msghdr is valid; the pointers set below outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = cmsg_space(fds.len()) as _;
        // SAFETY: the control buffer has room for one cmsghdr with fds.len() descriptors.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN((fds.len() * FD_SIZE) as u32) as _;
            let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
            for (i, fd) in fds.iter().enumerate() {
                data.add(i).write_unaligned(fd.as_raw_fd());
            }
        }
    }
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_NOSIGNAL;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    // SAFETY: msg points at valid buffers for the duration of the call.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, flags) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

/// Read into `buf`, appending any descriptors received to `fds` (close-on-exec).
/// Returns the number of bytes read; 0 means the peer closed the connection.
pub fn recv_with_fds(sock: BorrowedFd, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<usize> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut control = vec![0u64; cmsg_space(MAX_FDS_PER_SEND).div_ceil(8)];
    // SAFETY: as in send_with_fds.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = (control.len() * 8) as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    // SAFETY: msg points at valid buffers for the duration of the call.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, flags) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // Take ownership of every descriptor first, so none leaks on the error below.
    // SAFETY: walking the control messages recvmsg just filled in.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for i in 0..len / FD_SIZE {
                    let fd = OwnedFd::from_raw_fd(data.add(i).read_unaligned());
                    #[cfg(not(target_os = "linux"))]
                    libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
                    fds.push(fd);
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other(format!("more than {MAX_FDS_PER_SEND} descriptors in one send")));
    }
    Ok(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    fn temp_file(text: &str) -> std::fs::File {
        let path = std::env::temp_dir().join(format!("protocol-fd-{}-{text}", std::process::id()));
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
        f
    }

    #[test]
    fn descriptors_arrive_with_the_bytes_and_still_work() {
        let (a, b) = UnixStream::pair().unwrap();
        let (one, two) = (temp_file("one"), temp_file("two"));
        let sent = send_with_fds(a.as_fd(), b"hello", &[one.as_fd(), two.as_fd()]).unwrap();
        assert_eq!(sent, 5);
        let mut buf = [0u8; 16];
        let mut fds = Vec::new();
        let n = recv_with_fds(b.as_fd(), &mut buf, &mut fds).unwrap();
        assert_eq!(&buf[..n], b"hello");
        assert_eq!(fds.len(), 2);
        let mut text = String::new();
        let mut f = std::fs::File::from(fds.remove(1));
        f.rewind().unwrap();
        f.read_to_string(&mut text).unwrap();
        assert_eq!(text, "two");
        // SAFETY: F_GETFD on a descriptor we own.
        let flags = unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "received descriptors are close-on-exec");
    }

    #[test]
    fn plain_bytes_and_closed_peers() {
        let (a, b) = UnixStream::pair().unwrap();
        assert_eq!(send_with_fds(a.as_fd(), b"x", &[]).unwrap(), 1);
        let mut buf = [0u8; 4];
        let mut fds = Vec::new();
        assert_eq!(recv_with_fds(b.as_fd(), &mut buf, &mut fds).unwrap(), 1);
        assert!(fds.is_empty());
        drop(a);
        assert_eq!(recv_with_fds(b.as_fd(), &mut buf, &mut fds).unwrap(), 0);
    }
}
