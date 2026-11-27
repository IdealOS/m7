//! Sandboxes, applied between fork and exec: by session for each app's
//! `app-host`, and by importd for the `import-reader` that reads untrusted files.
//!
//! An app gets three layers, all set up before any of its code runs:
//!
//! - **A network namespace** of its own, with only loopback, unless it holds the
//!   `network` grant. (Grants are read when the app starts, so a new network
//!   grant takes effect when it next starts.)
//! - **Landlock**: it may read and execute only the system's program and library
//!   directories (and the program's own), and read and write only its runtime
//!   directory. Without the network grant it may not bind or connect TCP either
//!   (Landlock ABI 4+), and from ABI 6 it can't signal or reach the abstract Unix
//!   sockets of processes outside its sandbox.
//! - **seccomp**: no new sockets at all (app-host talks to the bus over the
//!   connection session hands it; a new Unix socket could reach the broker's
//!   socket as a system client), or only IPv4 and IPv6 ones with the network
//!   grant; and none of the calls that change mounts, namespaces, modules or
//!   other processes (`ptrace`, `mount`, `unshare`, `bpf`, `keyctl`, ...).
//!
//! Each layer works alone: seccomp keeps an app off the network even on a kernel
//! that can't make the namespace (an unprivileged session in a VM with user
//! namespaces turned off), and the namespace keeps it off even if a socket got
//! through. If the kernel has no Landlock, the app runs without file limits and
//! session says so.
//!
//! A helper like `import-reader` can be held tighter still: no directory it may
//! write at all, another Unix user (when started by root), and resource limits.
//!
//! Everything that can allocate or fail in interesting ways happens in
//! [`Sandbox::prepare`], in the parent; [`Sandbox::enter`] runs in the child after
//! fork and only makes system calls.

use std::path::{Path, PathBuf};

/// What a sandbox allows.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// Whether it may use the network (an app's `network` grant).
    pub network: bool,
    /// Its own directory: its working directory, and the one place it may write.
    /// None: it may write nowhere.
    pub runtime_dir: Option<PathBuf>,
    /// Directories and files it may read (and execute, for programs).
    pub read_exec: Vec<PathBuf>,
    /// The Unix user (and group) it runs as, when the parent is root. Ignored
    /// otherwise: only root can become someone else.
    pub user: Option<u32>,
    /// Resource limits, each set as both the soft and the hard limit.
    pub limits: Vec<(Limit, u64)>,
}

/// A resource a sandbox can limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Limit {
    /// Address space, in bytes (`RLIMIT_AS`).
    Memory,
    /// The largest file it may write, in bytes (`RLIMIT_FSIZE`).
    FileSize,
    /// Open descriptors (`RLIMIT_NOFILE`): one more than the highest it may have.
    OpenFiles,
    /// Core dumps, in bytes (`RLIMIT_CORE`).
    CoreSize,
}

/// The system paths every app may read and execute from, besides its program's
/// own directory: programs, libraries, the dynamic loader's cache and the time zone.
pub const SYSTEM_PATHS: &[&str] =
    &["/usr", "/lib", "/lib64", "/bin", "/sbin", "/opt/idealos", "/etc/ld.so.cache", "/etc/localtime"];

impl Spec {
    /// The usual sandbox for an app whose program is `program`.
    pub fn for_app(program: &Path, runtime_dir: PathBuf, network: bool) -> Spec {
        let mut read_exec: Vec<PathBuf> = SYSTEM_PATHS.iter().map(PathBuf::from).collect();
        if let Some(dir) = program_dir(program) {
            read_exec.push(dir);
        }
        Spec { network, runtime_dir: Some(runtime_dir), read_exec, user: None, limits: Vec::new() }
    }

    /// The tightest sandbox, for a helper whose program is `program` that works
    /// only on descriptors it's handed: no network, nowhere to write, running as
    /// `user` with `limits`.
    pub fn for_helper(program: &Path, user: u32, limits: Vec<(Limit, u64)>) -> Spec {
        let mut spec = Spec::for_app(program, PathBuf::new(), false);
        spec.runtime_dir = None;
        spec.user = Some(user);
        spec.limits = limits;
        spec
    }
}

/// The directory a program runs from: its own if it names one, else the first
/// directory on `PATH` that has it.
fn program_dir(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return std::fs::canonicalize(program).ok()?.parent().map(Path::to_owned);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .find(|d| d.join(program).is_file())
        .and_then(|d| std::fs::canonicalize(d).ok())
}

/// The runtime directory for app `app`: under `/run/idealos/apps` when session
/// may write there (on the device), else under `$XDG_RUNTIME_DIR` or the temp
/// directory.
pub fn runtime_dir(app: &str) -> PathBuf {
    let base = if is_writable(Path::new("/run")) {
        PathBuf::from("/run/idealos/apps")
    } else {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("idealos/apps")
    };
    base.join(app)
}

fn is_writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    // SAFETY: access(2) on a NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(target_os = "linux")]
pub use linux::Sandbox;

#[cfg(not(target_os = "linux"))]
pub use other::Sandbox;

#[cfg(not(target_os = "linux"))]
mod other {
    use super::Spec;
    use std::io;

    /// No sandboxes away from Linux (session only runs there; this keeps it building).
    pub struct Sandbox;

    impl Sandbox {
        pub fn prepare(spec: &Spec) -> io::Result<Sandbox> {
            if let Some(dir) = &spec.runtime_dir {
                std::fs::create_dir_all(dir)?;
            }
            Ok(Sandbox)
        }

        /// Whether file limits are on (never, here).
        pub fn limits_files(&self) -> bool {
            false
        }

        /// Whether it will run as another user (never, here).
        pub fn changes_user(&self) -> bool {
            false
        }

        /// # Safety
        /// Only between fork and exec.
        pub unsafe fn enter(&self) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::Spec;
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::DirBuilderExt;

    // Landlock (include/uapi/linux/landlock.h).
    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: u32 = 1;
    const FS_EXECUTE: u64 = 1 << 0;
    const FS_WRITE_FILE: u64 = 1 << 1;
    const FS_READ_FILE: u64 = 1 << 2;
    const FS_READ_DIR: u64 = 1 << 3;
    const NET_BIND_TCP: u64 = 1 << 0;
    const NET_CONNECT_TCP: u64 = 1 << 1;
    const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    const SCOPE_SIGNAL: u64 = 1 << 1;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// Every filesystem right Landlock ABI `abi` knows.
    fn fs_rights(abi: i64) -> u64 {
        match abi {
            1 => (1 << 13) - 1,
            2 => (1 << 14) - 1,
            3 | 4 => (1 << 15) - 1,
            _ => (1 << 16) - 1,
        }
    }

    // seccomp-bpf (linux/filter.h, linux/seccomp.h).
    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const JGE_K: u16 = 0x35;
    const JSET_K: u16 = 0x45;
    const RET_K: u16 = 0x06;
    const RET_ALLOW: u32 = 0x7fff_0000;
    const RET_ERRNO: u32 = 0x0005_0000;
    const RET_KILL_PROCESS: u32 = 0x8000_0000;
    /// Offsets into `struct seccomp_data`: the call number, the architecture, the
    /// first argument (its low 32 bits, little-endian).
    const NR: u32 = 0;
    const ARCH: u32 = 4;
    const ARG0: u32 = 16;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xC000_00B7;
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xC000_003E;
    /// The flags of `clone` that make namespaces.
    const CLONE_NAMESPACES: u32 = 0x7E02_0000;

    /// Calls no app needs, which change the system or other processes.
    const DENIED: &[libc::c_long] = &[
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_kexec_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_userfaultfd,
        libc::SYS_open_by_handle_at,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_acct,
        libc::SYS_quotactl,
    ];

    fn stmt(code: u16, k: u32) -> libc::sock_filter {
        libc::sock_filter { code, jt: 0, jf: 0, k }
    }

    fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
        libc::sock_filter { code, jt, jf, k }
    }

    /// The seccomp program: see the module docs.
    pub(super) fn filter(network: bool) -> Vec<libc::sock_filter> {
        let mut f = vec![
            stmt(LD_W_ABS, ARCH),
            jump(JEQ_K, AUDIT_ARCH, 1, 0),
            stmt(RET_K, RET_KILL_PROCESS),
            stmt(LD_W_ABS, NR),
        ];
        #[cfg(target_arch = "x86_64")]
        {
            // x32 calls, which would bypass the numbers below.
            f.push(jump(JGE_K, 0x4000_0000, 0, 1));
            f.push(stmt(RET_K, RET_KILL_PROCESS));
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = JGE_K;
        for &nr in DENIED {
            f.push(jump(JEQ_K, nr as u32, 0, 1));
            f.push(stmt(RET_K, RET_ERRNO | libc::EPERM as u32));
        }
        // clone3 can't be inspected (its flags are behind a pointer): ENOSYS makes
        // the C library fall back to clone, whose flags can be.
        f.push(jump(JEQ_K, libc::SYS_clone3 as u32, 0, 1));
        f.push(stmt(RET_K, RET_ERRNO | libc::ENOSYS as u32));
        let socket: Vec<libc::sock_filter> = if network {
            vec![
                stmt(LD_W_ABS, ARG0),
                jump(JEQ_K, libc::AF_INET as u32, 2, 0),
                jump(JEQ_K, libc::AF_INET6 as u32, 1, 0),
                stmt(RET_K, RET_ERRNO | libc::EACCES as u32),
                stmt(RET_K, RET_ALLOW),
            ]
        } else {
            vec![stmt(RET_K, RET_ERRNO | libc::EACCES as u32)]
        };
        f.push(jump(JEQ_K, libc::SYS_socket as u32, 0, socket.len() as u8));
        f.extend(socket);
        f.extend([
            jump(JEQ_K, libc::SYS_clone as u32, 0, 3),
            stmt(LD_W_ABS, ARG0),
            jump(JSET_K, CLONE_NAMESPACES, 0, 1),
            stmt(RET_K, RET_ERRNO | libc::EPERM as u32),
            stmt(RET_K, RET_ALLOW),
        ]);
        f
    }

    pub struct Sandbox {
        network: bool,
        runtime_dir: Option<OwnedFd>,
        /// The user to become: only when the parent is root.
        user: Option<libc::uid_t>,
        limits: Vec<(i32, libc::rlimit)>,
        /// The Landlock ruleset, ready to apply, if the kernel has Landlock.
        ruleset: Option<OwnedFd>,
        filter: Vec<libc::sock_filter>,
    }

    fn resource(limit: super::Limit) -> i32 {
        use super::Limit;
        (match limit {
            Limit::Memory => libc::RLIMIT_AS,
            Limit::FileSize => libc::RLIMIT_FSIZE,
            Limit::OpenFiles => libc::RLIMIT_NOFILE,
            Limit::CoreSize => libc::RLIMIT_CORE,
        }) as i32
    }

    fn cstr(path: &std::path::Path) -> io::Result<CString> {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::other("a path with a NUL"))
    }

    fn open_path(path: &std::path::Path) -> io::Result<OwnedFd> {
        let c = cstr(path)?;
        // SAFETY: open(2) on a NUL-terminated path; the descriptor is owned below.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor open(2) just returned, owned by nothing else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    impl Sandbox {
        /// Make the runtime directory and build the Landlock ruleset and the
        /// seccomp program, in session, before forking.
        pub fn prepare(spec: &Spec) -> io::Result<Sandbox> {
            let dir = match &spec.runtime_dir {
                Some(dir) => {
                    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
                    Some(open_path(dir)?)
                }
                None => None,
            };
            // SAFETY: geteuid(2) takes nothing and can't fail.
            let root = unsafe { libc::geteuid() } == 0;
            Ok(Sandbox {
                network: spec.network,
                runtime_dir: dir,
                user: spec.user.filter(|_| root),
                limits: spec
                    .limits
                    .iter()
                    .map(|&(l, n)| (resource(l), libc::rlimit { rlim_cur: n as _, rlim_max: n as _ }))
                    .collect(),
                ruleset: ruleset(spec)?,
                filter: filter(spec.network),
            })
        }

        /// Whether file limits are on (the kernel has Landlock).
        pub fn limits_files(&self) -> bool {
            self.ruleset.is_some()
        }

        /// Whether it will run as another user (the spec named one and the
        /// parent is root).
        pub fn changes_user(&self) -> bool {
            self.user.is_some()
        }

        /// Enter the sandbox: resource limits, the network namespace, the runtime
        /// directory, another user, Landlock, then seccomp.
        ///
        /// # Safety
        /// Only between fork and exec (a `pre_exec` hook): it makes system calls
        /// and nothing else.
        pub unsafe fn enter(&self) -> io::Result<()> {
            let fail = || Err(io::Error::last_os_error());
            // SAFETY (the block): plain system calls on descriptors and structs
            // this sandbox owns.
            unsafe {
                for (resource, limit) in &self.limits {
                    if libc::setrlimit(*resource as _, limit) != 0 {
                        return fail();
                    }
                }
                // The namespace first: as root it needs no user namespace.
                if !self.network {
                    isolate_network();
                }
                if let Some(dir) = &self.runtime_dir
                    && libc::fchdir(dir.as_raw_fd()) != 0
                {
                    return fail();
                }
                if let Some(uid) = self.user {
                    // Groups first: once it isn't root it can't change them.
                    if libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setgid(uid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return fail();
                    }
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return fail();
                }
                if let Some(rs) = &self.ruleset
                    && libc::syscall(libc::SYS_landlock_restrict_self, rs.as_raw_fd(), 0) != 0
                {
                    return fail();
                }
                let prog = libc::sock_fprog {
                    len: self.filter.len() as u16,
                    filter: self.filter.as_ptr() as *mut _,
                };
                if libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER,
                    &prog as *const _ as usize,
                    0,
                    0,
                ) != 0
                {
                    return fail();
                }
            }
            Ok(())
        }
    }

    /// Move into a new network namespace with only loopback, up: as root (on the
    /// device), or in a user namespace (a developer's session). Neither working
    /// leaves seccomp keeping the app off the network. Returns whether it moved.
    ///
    /// # Safety
    /// Only in a single-threaded process (between fork and exec).
    pub(super) unsafe fn isolate_network() -> bool {
        // SAFETY: unshare(2) with namespace flags.
        let moved = unsafe {
            libc::unshare(libc::CLONE_NEWNET) == 0
                || libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) == 0
        };
        if moved {
            // SAFETY: as above.
            unsafe { loopback_up() };
        }
        moved
    }

    /// Bring up `lo` in a new network namespace (it starts down).
    unsafe fn loopback_up() {
        #[repr(C)]
        struct IfReq {
            name: [u8; 16],
            flags: libc::c_short,
            _pad: [u8; 22],
        }
        // SAFETY: a socket and two ioctls on a struct the size the kernel expects.
        unsafe {
            let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
            if s < 0 {
                return;
            }
            let mut req = IfReq { name: [0; 16], flags: 0, _pad: [0; 22] };
            req.name[..2].copy_from_slice(b"lo");
            if libc::ioctl(s, libc::SIOCGIFFLAGS as _, &mut req) == 0 {
                req.flags |= libc::IFF_UP as libc::c_short;
                libc::ioctl(s, libc::SIOCSIFFLAGS as _, &mut req);
            }
            libc::close(s);
        }
    }

    /// The Landlock ruleset for `spec`, or `None` if the kernel has no Landlock.
    fn ruleset(spec: &Spec) -> io::Result<Option<OwnedFd>> {
        // SAFETY: asking for the ABI version takes no struct.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<u8>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if abi < 1 {
            return Ok(None);
        }
        let fs = fs_rights(abi);
        let attr = RulesetAttr {
            handled_access_fs: fs,
            handled_access_net: if abi >= 4 && !spec.network { NET_BIND_TCP | NET_CONNECT_TCP } else { 0 },
            scoped: if abi >= 6 { SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL } else { 0 },
        };
        let size = match abi {
            1..=3 => 8,
            4 | 5 => 16,
            _ => 24,
        };
        // SAFETY: a ruleset attr struct of at least `size` bytes.
        let fd = unsafe {
            libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, size, 0u32)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor the kernel just returned, owned by nothing else.
        let rs = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let add = |path: &std::path::Path, dir_rights: u64, file_rights: u64| -> io::Result<()> {
            let Ok(meta) = std::fs::metadata(path) else { return Ok(()) }; // absent here
            let target = open_path(path)?;
            let rule = PathBeneath {
                allowed_access: (if meta.is_dir() { dir_rights } else { file_rights }) & fs,
                parent_fd: target.as_raw_fd(),
            };
            // SAFETY: a path-beneath rule struct and descriptors we own.
            let r = unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    rs.as_raw_fd(),
                    RULE_PATH_BENEATH,
                    &rule as *const PathBeneath,
                    0u32,
                )
            };
            if r != 0 {
                Err(io::Error::other(format!("{}: {}", path.display(), io::Error::last_os_error())))
            } else {
                Ok(())
            }
        };
        for path in &spec.read_exec {
            add(path, FS_EXECUTE | FS_READ_FILE | FS_READ_DIR, FS_EXECUTE | FS_READ_FILE)?;
        }
        add(std::path::Path::new("/dev/null"), 0, FS_READ_FILE | FS_WRITE_FILE)?;
        add(std::path::Path::new("/dev/urandom"), 0, FS_READ_FILE)?;
        if let Some(dir) = &spec.runtime_dir {
            add(dir, fs, fs)?;
        }
        Ok(Some(rs))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The interfaces a process can see.
        fn interfaces() -> Vec<String> {
            // SAFETY: if_nameindex's list is read and then freed.
            unsafe {
                let list = libc::if_nameindex();
                assert!(!list.is_null());
                let mut names = Vec::new();
                let mut p = list;
                while (*p).if_index != 0 {
                    names.push(std::ffi::CStr::from_ptr((*p).if_name).to_string_lossy().into_owned());
                    p = p.add(1);
                }
                libc::if_freenameindex(list);
                names
            }
        }

        /// Needs root, or user namespaces: `sudo` it where those are off.
        #[test]
        fn without_the_network_grant_only_loopback_is_left() {
            // SAFETY: fork, then only system calls and _exit in the child.
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                // SAFETY: the child is single-threaded.
                let code = if !unsafe { isolate_network() } {
                    3
                } else if interfaces() == ["lo"] {
                    0
                } else {
                    1
                };
                // SAFETY: ends the child without running the parent's destructors.
                unsafe { libc::_exit(code) };
            }
            let mut status = 0;
            // SAFETY: waiting for the child we forked.
            unsafe { libc::waitpid(pid, &mut status, 0) };
            match libc::WEXITSTATUS(status) {
                0 => {}
                3 => eprintln!("skipped: no network namespace without root or user namespaces"),
                other => panic!("the child saw more than loopback ({other}); outside: {:?}", interfaces()),
            }
        }

        #[test]
        fn the_filter_has_every_jump_inside_it() {
            for network in [false, true] {
                let f = filter(network);
                for (i, s) in f.iter().enumerate() {
                    if s.code & 0x07 == 0x05 {
                        assert!(i + 1 + s.jt as usize <= f.len() && i + 1 + s.jf as usize <= f.len(), "{i}");
                    }
                }
                assert_eq!(f.last().unwrap().k, RET_ALLOW);
            }
        }
    }
}

/// Probes run inside a sandbox by the tests below: the test binary runs itself
/// again with `SANDBOX_PROBE` set, sandboxed, and that run reports what worked.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// What a sandboxed process could do, one letter each: `f` read a file
    /// outside, `w` write in its runtime directory, `i` make an IPv4 socket, `u`
    /// a Unix socket, `p` ptrace, `n` make a namespace, `l` have at most 64
    /// descriptors (a limit was set).
    fn probe() -> String {
        let mut could = String::new();
        if std::fs::read("/etc/hostname").is_ok() || std::fs::read("/etc/passwd").is_ok() {
            could.push('f');
        }
        if std::fs::write("written", b"x").is_ok() {
            could.push('w');
        }
        // SAFETY: plain system calls; any descriptor made is closed.
        unsafe {
            let s = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            if s >= 0 {
                could.push('i');
                libc::close(s);
            }
            let s = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            if s >= 0 {
                could.push('u');
                libc::close(s);
            }
            if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) == 0 {
                could.push('p');
            }
            if libc::unshare(libc::CLONE_NEWUSER) == 0 {
                could.push('n');
            }
            let mut files = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut files) == 0 && files.rlim_cur <= 64 {
                could.push('l');
            }
        }
        could
    }

    #[test]
    fn sandbox_probe() {
        if std::env::var_os("SANDBOX_PROBE").is_some() {
            print!("PROBE[{}]", probe());
        }
    }

    /// Run the probe in an app's sandbox, or with `helper` a helper's (working in
    /// the same directory, which it may not write); returns what it could do.
    fn run_probe(network: bool, helper: bool) -> String {
        let me = std::env::current_exe().unwrap();
        let dir =
            std::env::temp_dir().join(format!("sandbox-test-{}-{network}-{helper}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let spec = if helper {
            std::fs::create_dir_all(&dir).unwrap();
            Spec::for_helper(&me, 61_000, vec![(Limit::OpenFiles, 64), (Limit::CoreSize, 0)])
        } else {
            Spec::for_app(&me, dir.clone(), network)
        };
        let sandbox = Sandbox::prepare(&spec).unwrap();
        assert!(sandbox.limits_files(), "this kernel has no Landlock");
        let mut cmd = Command::new(&me);
        cmd.args(["--exact", "tests::sandbox_probe", "--nocapture", "--test-threads", "1"])
            .env("SANDBOX_PROBE", "1")
            .current_dir(&dir);
        // SAFETY: enter only makes system calls.
        unsafe { cmd.pre_exec(move || sandbox.enter()) };
        let out = cmd.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        let start = text
            .find("PROBE[")
            .unwrap_or_else(|| panic!("no probe output: {text} {}", String::from_utf8_lossy(&out.stderr)));
        text[start + 6..].split(']').next().unwrap().to_owned()
    }

    #[test]
    fn apps_cannot_open_files_or_sockets_they_were_not_given() {
        assert_eq!(run_probe(false, false), "w", "only writing its own directory");
    }

    #[test]
    fn the_network_grant_allows_internet_sockets_only() {
        assert_eq!(run_probe(true, false), "wi");
    }

    #[test]
    fn helpers_cannot_write_anywhere_and_keep_their_limits() {
        assert_eq!(run_probe(false, true), "l");
    }
}
