//! Finding backup drives and mounting them.
//!
//! On the device, a drive is any block device (a USB stick's partition, or a whole
//! unpartitioned stick) whose first sector says exFAT, that nothing has mounted.
//! backupd mounts it under `/run/idealos/backup/<id>` only while it reads or writes,
//! so it's safe to unplug the rest of the time. For development and tests a plain
//! directory stands in for one drive, `dir`.

use std::path::{Path, PathBuf};

/// A drive that's plugged in.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub id: String,
    pub bytes: u64,
}

/// Where backupd looks for drives.
#[derive(Debug, Clone)]
pub enum Drives {
    /// Block devices, mounted under `mounts`.
    Block { mounts: PathBuf },
    /// A directory standing in for one drive, `dir`.
    Dir(PathBuf),
}

/// A mounted drive, unmounted when dropped.
pub struct Mounted {
    pub path: PathBuf,
    unmount: bool,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if self.unmount {
            sys::unmount(&self.path);
        }
    }
}

impl Drives {
    /// Unmount drives a previous run of backupd left mounted (it stopped mid-copy).
    pub fn release_stale(&self) {
        if let Drives::Block { mounts } = self {
            let table = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
            for target in table.lines().filter_map(|l| l.split(' ').nth(1)) {
                if Path::new(target).starts_with(mounts) {
                    sys::unmount(Path::new(target));
                }
            }
        }
    }

    /// The drives plugged in now.
    pub fn list(&self) -> Vec<Found> {
        match self {
            Drives::Block { .. } => sys::exfat_devices(),
            Drives::Dir(path) => {
                vec![Found { id: "dir".into(), bytes: space(path).map_or(0, |(total, _)| total) }]
            }
        }
    }

    /// Mount drive `id` (until the result is dropped).
    pub fn mount(&self, id: &str) -> Result<Mounted, String> {
        match self {
            Drives::Dir(path) if id == "dir" => Ok(Mounted { path: path.clone(), unmount: false }),
            Drives::Dir(_) => Err(format!("no drive {id}")),
            Drives::Block { mounts } => {
                if !sys::exfat_devices().iter().any(|d| d.id == id) {
                    return Err(format!("no drive {id}"));
                }
                let path = mounts.join(id);
                std::fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                sys::mount(id, &path)?;
                Ok(Mounted { path, unmount: true })
            }
        }
    }
}

/// (total, free) bytes of the file system holding `path`.
pub fn space(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `st` a valid statvfs to fill.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let block = st.f_frsize as u64;
    Some((st.f_blocks as u64 * block, st.f_bavail as u64 * block))
}

/// Whether a device's first sector is an exFAT boot sector.
pub fn is_exfat(sector: &[u8]) -> bool {
    sector.len() >= 11 && &sector[3..11] == b"EXFAT   "
}

#[cfg(target_os = "linux")]
mod sys {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::{Found, is_exfat};

    /// Unmounted block devices holding exFAT.
    pub fn exfat_devices() -> Vec<Found> {
        let mounted = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/sys/class/block") else { return out };
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            if ["loop", "ram", "zram", "dm-"].iter().any(|p| id.starts_with(p)) {
                continue;
            }
            let dev = format!("/dev/{id}");
            // Mounted by someone else (the system's own disk, say). Our own mount
            // point doesn't count: a drive stays listed while we use it.
            let theirs = mounted.lines().any(|l| {
                let mut f = l.split(' ');
                f.next() == Some(&dev) && !f.next().unwrap_or("").starts_with("/run/idealos/backup/")
            });
            if theirs {
                continue;
            }
            let mut sector = [0u8; 512];
            let read = std::fs::File::open(&dev).and_then(|mut f| f.read_exact(&mut sector));
            if read.is_err() || !is_exfat(&sector) {
                continue;
            }
            let sectors: u64 = std::fs::read_to_string(entry.path().join("size"))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            out.push(Found { id, bytes: sectors * 512 });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    pub fn mount(id: &str, at: &Path) -> Result<(), String> {
        let dev = std::ffi::CString::new(format!("/dev/{id}")).map_err(|e| e.to_string())?;
        let target = std::ffi::CString::new(at.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        let fstype = c"exfat";
        let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_NOATIME;
        // SAFETY: valid C strings; no mount data.
        let r =
            unsafe { libc::mount(dev.as_ptr(), target.as_ptr(), fstype.as_ptr(), flags, std::ptr::null()) };
        if r != 0 {
            return Err(format!("mounting {id}: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    pub fn unmount(at: &Path) {
        let Ok(target) = std::ffi::CString::new(at.as_os_str().as_bytes()) else { return };
        // SAFETY: a valid C string. Unmounting writes back everything cached.
        if unsafe { libc::umount2(target.as_ptr(), 0) } != 0 {
            log::warn!("backupd: unmounting {}: {}", at.display(), std::io::Error::last_os_error());
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod sys {
    use std::path::Path;

    use super::Found;

    pub fn exfat_devices() -> Vec<Found> {
        Vec::new()
    }

    pub fn mount(id: &str, _at: &Path) -> Result<(), String> {
        Err(format!("mounting {id}: drives are only mounted on Linux"))
    }

    pub fn unmount(_at: &Path) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exfat_is_known_by_its_boot_sector() {
        let mut sector = [0u8; 512];
        sector[3..11].copy_from_slice(b"EXFAT   ");
        assert!(is_exfat(&sector));
        sector[3..11].copy_from_slice(b"NTFS    ");
        assert!(!is_exfat(&sector));
        assert!(!is_exfat(&[0u8; 4]));
    }

    #[test]
    fn a_directory_stands_in_for_a_drive() {
        let d = Drives::Dir(std::env::temp_dir());
        assert_eq!(d.list()[0].id, "dir");
        assert_eq!(d.mount("dir").unwrap().path, std::env::temp_dir());
        assert!(d.mount("sda1").is_err());
    }
}
