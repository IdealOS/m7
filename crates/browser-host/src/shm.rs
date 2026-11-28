//! Shared buffers: files with no name, which only the descriptors passed over
//! the bus can reach.

use std::fs::File;
use std::io;

/// A new, empty shared buffer of `len` bytes.
pub fn buffer(len: u64) -> io::Result<File> {
    let file = anonymous()?;
    file.set_len(len)?;
    Ok(file)
}

#[cfg(target_os = "linux")]
fn anonymous() -> io::Result<File> {
    use std::os::fd::FromRawFd;
    // A memfd: memory, with no path on any filesystem.
    let fd = unsafe { libc::memfd_create(c"surface".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
fn anonymous() -> io::Result<File> {
    // macOS has no memfd: a file in the temporary directory, removed at once,
    // so only its descriptors reach it.
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "browser-host-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt;

    #[test]
    fn a_buffer_holds_what_is_written_into_it() {
        let b = super::buffer(16).unwrap();
        assert_eq!(b.metadata().unwrap().len(), 16);
        b.write_all_at(&[7; 4], 12).unwrap();
        let mut back = [0; 16];
        b.read_exact_at(&mut back, 0).unwrap();
        assert_eq!(&back[12..], [7; 4]);
    }
}
