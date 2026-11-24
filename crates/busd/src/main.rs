//! `busd [--socket PATH]`: run the bus broker.
//!
//! Listens on `protocol::bus::default_socket_path()` unless told otherwise. The
//! socket's directory is created owner-only, and the broker also checks each
//! client's uid, so only the same user (or root) can reach it. Stops on SIGTERM or
//! SIGINT and removes its socket.
//!
//! Started by `session`, it also inherits session's own connection, as the
//! descriptor named in `IDEALOS_BUS_SESSION_FD`. Only that connection may hand the
//! broker connections for services and apps (the broker's `adopt` method).

use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let path = match parse_args(std::env::args().skip(1)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("busd: {e}\nusage: busd [--socket PATH]");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    if let Err(e) = runtime.block_on(run(&path)) {
        log::error!("busd: {e}");
        std::process::exit(1);
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<PathBuf, String> {
    let mut path = protocol::bus::default_socket_path();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => path = args.next().ok_or("--socket needs a path")?.into(),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(path)
}

/// Names the descriptor of session's connection, which busd inherits.
const SESSION_FD_ENV: &str = "IDEALOS_BUS_SESSION_FD";

async fn run(path: &Path) -> Result<(), String> {
    match busd::raise_fd_limit() {
        Ok(n) => log::debug!("busd: up to {n} open descriptors"),
        Err(e) => log::warn!("busd: cannot raise the open-descriptor limit: {e}"),
    }
    let session = session_connection()?;
    // Handlers first: once the socket exists, a SIGTERM must clean it up, not kill
    // busd before it can.
    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let listener = bind(path)?;
    log::info!("busd: listening on {}", path.display());
    let result = tokio::select! {
        r = busd::serve(listener, Box::new(busd::policy::Grants::new()), session) => r.map_err(|e| format!("accept: {e}")),
        _ = term.recv() => { log::info!("busd: stopping on SIGTERM"); Ok(()) }
        _ = int.recv() => { log::info!("busd: stopping on SIGINT"); Ok(()) }
    };
    let _ = std::fs::remove_file(path);
    result
}

/// Session's connection, if session started this broker.
fn session_connection() -> Result<Option<tokio::net::UnixStream>, String> {
    let Some(text) = std::env::var_os(SESSION_FD_ENV) else { return Ok(None) };
    let fd: RawFd = text
        .to_str()
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| format!("{SESSION_FD_ENV} is not a descriptor number: {text:?}"))?;
    // SAFETY: fstat into a zeroed struct; any descriptor number is safe to ask about.
    let is_socket = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFSOCK
    };
    if !is_socket {
        return Err(format!("{SESSION_FD_ENV}={fd} is not an open socket"));
    }
    // SAFETY: session passed this descriptor for exactly this use; it's wrapped
    // once, here. Close-on-exec again, so nothing busd starts inherits it.
    let stream = unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        std::os::unix::net::UnixStream::from(OwnedFd::from_raw_fd(fd))
    };
    stream.set_nonblocking(true).map_err(|e| e.to_string())?;
    tokio::net::UnixStream::from_std(stream).map(Some).map_err(|e| e.to_string())
}

/// Bind `path`, replacing a socket left behind by a broker that died, but not one
/// that a running broker still answers on.
fn bind(path: &Path) -> Result<UnixListener, String> {
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(format!("another broker is already listening on {}", path.display()));
        }
        std::fs::remove_file(path).map_err(|e| format!("remove stale {}: {e}", path.display()))?;
    }
    let listener = UnixListener::bind(path).map_err(|e| format!("bind {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(listener)
}
