//! Opening a source to read files from: a share mounted read-only for as long as
//! it's needed, or a directory under the root importd allows. Paths inside a
//! source are relative, `/`-separated and can't climb out of it; symbolic links
//! and hidden files (macOS's `.DS_Store` and `._` files) are passed over.

use std::path::{Component, Path, PathBuf};

use protocol::import::{Entry, Kind};

use crate::kinds::kind_of;

/// An open source, readable under `root` until dropped (a share is unmounted).
pub struct Opened {
    pub root: PathBuf,
    mounted: bool,
}

impl Drop for Opened {
    fn drop(&mut self) {
        if self.mounted {
            sys::unmount(&self.root);
            let _ = std::fs::remove_dir(&self.root);
        }
    }
}

impl Opened {
    /// A directory, which must be `allowed` or inside it.
    pub fn dir(path: &str, allowed: Option<&Path>) -> Result<Opened, String> {
        let Some(allowed) = allowed else {
            return Err("folders on this machine can't be imported from (importd has no --dir)".into());
        };
        let real = std::fs::canonicalize(path).map_err(|e| format!("{path}: {e}"))?;
        let root = std::fs::canonicalize(allowed).map_err(|e| format!("{}: {e}", allowed.display()))?;
        if !real.starts_with(&root) {
            return Err(format!("{path} isn't inside {}", root.display()));
        }
        Ok(Opened { root: real, mounted: false })
    }

    /// Share `share` on `host` (at `port`, if not 445), mounted read-only at `at`
    /// as `user` with `password`.
    pub fn share(share: &Share, password: &str, at: PathBuf) -> Result<Opened, String> {
        std::fs::create_dir_all(&at).map_err(|e| format!("{}: {e}", at.display()))?;
        if let Err(e) = sys::mount_cifs(share, password, &at) {
            let _ = std::fs::remove_dir(&at);
            return Err(e);
        }
        Ok(Opened { root: at, mounted: true })
    }

    /// The real path of `path` inside the source.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        Ok(self.root.join(relative(path)?))
    }

    /// Open file `path` in the source to read. Nothing on the way may be a
    /// symbolic link or lead out of the source, checked by the kernel as it opens
    /// (so a server can't swap a file for a link between listing and opening),
    /// and it must be an ordinary file.
    pub fn open(&self, path: &str) -> Result<std::fs::File, String> {
        open_in(&self.root, path)
    }
}

/// [`Opened::open`] for a source open at `root`.
pub fn open_in(root: &Path, path: &str) -> Result<std::fs::File, String> {
    let file = sys::open_beneath(root, &relative(path)?).map_err(|e| match e.raw_os_error() {
        Some(libc::ELOOP) | Some(libc::EXDEV) => "it's a link, or leads out of the source".to_owned(),
        _ => e.to_string(),
    })?;
    match file.metadata() {
        Ok(m) if m.is_file() => Ok(file),
        Ok(_) => Err("it isn't an ordinary file".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// What mounting a share needs.
#[derive(Debug, Clone)]
pub struct Share {
    pub host: String,
    pub share: String,
    pub user: String,
    pub port: u16,
}

/// `path` as a relative path that stays inside the source.
pub fn relative(path: &str) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();
    for c in Path::new(path.trim_matches('/')).components() {
        match c {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return Err(format!("{path:?} isn't a path inside the source")),
        }
    }
    Ok(out)
}

fn hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// The entries of folder `dir`: folders first, then files, each by name.
pub fn list(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
        if hidden(&name) || meta.file_type().is_symlink() {
            continue;
        }
        let (kind, size) = if meta.is_dir() { (Kind::Folder, 0) } else { (kind_of(&name), meta.len()) };
        if meta.is_dir() || meta.is_file() {
            entries.push(Entry { name, kind, size });
        }
    }
    entries.sort_by(|a, b| (a.kind != Kind::Folder, &a.name).cmp(&(b.kind != Kind::Folder, &b.name)));
    Ok(entries)
}

/// A file to import: its path in the source, its size and kind. (It's opened
/// by its path, with [`Opened::open`].)
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub path: String,
    pub size: u64,
    pub kind: Kind,
}

/// Every file in `paths` of the source, folders gone through to the bottom, in
/// order and each once.
pub fn walk(opened: &Opened, paths: &[String]) -> Result<Vec<File>, String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for p in paths {
        let rel = relative(p)?;
        let real = opened.root.join(&rel);
        let meta = std::fs::symlink_metadata(&real).map_err(|e| format!("{p}: {e}"))?;
        add(&rel, &real, &meta, &mut out, &mut seen);
    }
    Ok(out)
}

fn add(
    rel: &Path,
    real: &Path,
    meta: &std::fs::Metadata,
    out: &mut Vec<File>,
    seen: &mut std::collections::HashSet<PathBuf>,
) {
    if meta.file_type().is_symlink() || !seen.insert(rel.to_owned()) {
        return;
    }
    if meta.is_file() {
        let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push(File {
            path: rel.to_string_lossy().replace('\\', "/"),
            size: meta.len(),
            kind: kind_of(&name),
        });
    } else if meta.is_dir() {
        let Ok(entries) = std::fs::read_dir(real) else { return };
        let mut children: Vec<_> = entries.flatten().collect();
        children.sort_by_key(|e| e.file_name());
        for e in children {
            let name = e.file_name().to_string_lossy().into_owned();
            if hidden(&name) {
                continue;
            }
            if let Ok(m) = std::fs::symlink_metadata(e.path()) {
                add(&rel.join(&name), &e.path(), &m, out, seen);
            }
        }
    }
}

/// Unmount shares a previous run left mounted under `mounts` (it stopped mid-import).
pub fn release_stale(mounts: &Path) {
    let table = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    for target in table.lines().filter_map(|l| l.split(' ').nth(1)) {
        if Path::new(target).starts_with(mounts) {
            sys::unmount(Path::new(target));
        }
    }
}

/// The options for the kernel's CIFS client: read-only, as `user`, the password
/// with its commas doubled (how the kernel reads a literal comma). `nohandlecache`
/// turns off caching directory handles: on Linux 6.12 freeing that cache could hang
/// the unmount forever (seen against macOS File Sharing).
pub fn cifs_options(share: &Share, password: &str, ip: &str) -> String {
    let escape = |s: &str| s.replace(',', ",,");
    format!(
        "username={},password={},ip={ip},port={},vers=3.0,iocharset=utf8,ro,noperm,nohandlecache",
        escape(&share.user),
        escape(password),
        share.port
    )
}

#[cfg(target_os = "linux")]
mod sys {
    use std::net::ToSocketAddrs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::{Share, cifs_options};

    pub fn mount_cifs(share: &Share, password: &str, at: &Path) -> Result<(), String> {
        // The kernel wants the address; mount.cifs would look the name up first.
        let ip = (share.host.as_str(), share.port)
            .to_socket_addrs()
            .map_err(|e| format!("{}: {e}", share.host))?
            .next()
            .ok_or_else(|| format!("{}: no address", share.host))?
            .ip()
            .to_string();
        let source =
            std::ffi::CString::new(format!("//{}/{}", share.host, share.share)).map_err(|e| e.to_string())?;
        let target = std::ffi::CString::new(at.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        let data = std::ffi::CString::new(cifs_options(share, password, &ip)).map_err(|e| e.to_string())?;
        let flags = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        // SAFETY: valid C strings for the source, target, type and options.
        let r = unsafe {
            libc::mount(source.as_ptr(), target.as_ptr(), c"cifs".as_ptr(), flags, data.as_ptr().cast())
        };
        if r != 0 {
            let e = std::io::Error::last_os_error();
            let why = match e.raw_os_error() {
                Some(libc::EACCES) | Some(libc::EPERM) => "the user name or password was refused".to_owned(),
                Some(libc::ENOENT) => "no such share".to_owned(),
                Some(libc::EHOSTDOWN) | Some(libc::EHOSTUNREACH) | Some(libc::ECONNREFUSED) => {
                    format!("{} isn't answering (is File Sharing on?)", share.host)
                }
                Some(libc::ENODEV) => "this system can't mount shares (no CIFS in the kernel)".to_owned(),
                _ => e.to_string(),
            };
            return Err(format!("mounting //{}/{}: {why}", share.host, share.share));
        }
        Ok(())
    }

    /// Open `rel` under `root` with openat2(2): beneath it, no links at all.
    pub fn open_beneath(root: &Path, rel: &Path) -> std::io::Result<std::fs::File> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let dir = std::fs::File::open(root)?;
        let path = std::ffi::CString::new(rel.as_os_str().as_bytes())?;
        // SAFETY: open_how is plain data; every field is set below.
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NONBLOCK) as u64;
        how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS;
        // SAFETY: openat2(2) with a directory we hold, a C string and an open_how of its size.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dir.as_raw_fd(),
                path.as_ptr(),
                &how as *const libc::open_how,
                size_of::<libc::open_how>(),
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a descriptor the kernel just returned, owned by nothing else.
        Ok(unsafe { std::fs::File::from_raw_fd(fd as i32) })
    }

    pub fn unmount(at: &Path) {
        let Ok(target) = std::ffi::CString::new(at.as_os_str().as_bytes()) else { return };
        // Detached: it goes from view at once and the kernel finishes when it can,
        // so a server that stops answering can't hold importd up.
        // SAFETY: a valid C string.
        if unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) } != 0 {
            log::warn!("importd: unmounting {}: {}", at.display(), std::io::Error::last_os_error());
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod sys {
    use std::path::Path;

    use super::Share;

    pub fn mount_cifs(share: &Share, _password: &str, _at: &Path) -> Result<(), String> {
        Err(format!("mounting //{}/{}: shares are only mounted on Linux", share.host, share.share))
    }

    pub fn unmount(_at: &Path) {}

    /// Away from Linux (development only): no link as the file itself, and its
    /// real path inside `root`. Not race-free, unlike openat2 on Linux.
    pub fn open_beneath(root: &Path, rel: &Path) -> std::io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = root.join(rel);
        if !std::fs::canonicalize(&path)?.starts_with(std::fs::canonicalize(root)?) {
            return Err(std::io::Error::from_raw_os_error(libc::EXDEV));
        }
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_source() {
        assert_eq!(relative("").unwrap(), PathBuf::new());
        assert_eq!(relative("/Music/./Albums/").unwrap(), PathBuf::from("Music/Albums"));
        for bad in ["../etc", "a/../../b", "a/.."] {
            assert!(relative(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn passwords_with_commas_survive_the_options() {
        let s = Share { host: "10.0.2.2".into(), share: "Music".into(), user: "josh".into(), port: 445 };
        let o = cifs_options(&s, "a,b", "10.0.2.2");
        assert!(o.starts_with("username=josh,password=a,,b,ip=10.0.2.2,port=445,"), "{o}");
        assert!(o.contains(",ro,") && o.ends_with(",nohandlecache"));
    }

    #[test]
    fn folders_list_and_walk_without_hidden_files_or_links() {
        let dir = std::env::temp_dir().join(format!("importd-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Notes/Old")).unwrap();
        std::fs::write(dir.join("Notes/a.md"), "# A").unwrap();
        std::fs::write(dir.join("Notes/Old/b.txt"), "b").unwrap();
        std::fs::write(dir.join("Notes/.DS_Store"), "junk").unwrap();
        std::fs::write(dir.join("song.mp3"), "id3").unwrap();
        std::os::unix::fs::symlink("/etc", dir.join("Notes/etc")).unwrap();

        let opened = Opened::dir(dir.to_str().unwrap(), Some(&dir)).unwrap();
        let top = list(&opened.root).unwrap();
        let names: Vec<(&str, Kind)> = top.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert_eq!(names, [("Notes", Kind::Folder), ("song.mp3", Kind::Audio)]);
        let files = walk(&opened, &["Notes".into(), "Notes/a.md".into(), "song.mp3".into()]).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            ["Notes/Old/b.txt", "Notes/a.md", "song.mp3"],
            "each once, no link, no hidden file"
        );
        assert!(walk(&opened, &["../x".into()]).is_err());

        // Opening: files inside, never a link, a folder or a way out.
        assert!(opened.open("Notes/a.md").is_ok());
        std::os::unix::fs::symlink(dir.join("Notes/a.md"), dir.join("Notes/swapped.md")).unwrap();
        let e = opened.open("Notes/swapped.md").unwrap_err();
        assert!(e.contains("link"), "{e}");
        assert!(opened.open("Notes/etc/passwd").is_err(), "through a linked folder");
        assert!(opened.open("Notes").is_err(), "a folder");
        assert!(opened.open("../x").is_err());
        assert!(Opened::dir("/", Some(&dir)).is_err(), "outside the allowed root");
        assert!(Opened::dir(dir.to_str().unwrap(), None).is_err(), "no root, no folders");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
