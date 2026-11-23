//! `busd [--socket PATH]`: run the bus broker.
//!
//! Listens on `protocol::bus::default_socket_path()` unless told otherwise. The
//! socket's directory is created owner-only, and the broker also checks each
//! client's uid, so only the same user (or root) can reach it. Stops on SIGTERM or
//! SIGINT and removes its socket.

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

async fn run(path: &Path) -> Result<(), String> {
    let listener = bind(path)?;
    log::info!("busd: listening on {}", path.display());
    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let result = tokio::select! {
        r = busd::serve(listener, Box::new(busd::AllowAll)) => r.map_err(|e| format!("accept: {e}")),
        _ = term.recv() => { log::info!("busd: stopping on SIGTERM"); Ok(()) }
        _ = int.recv() => { log::info!("busd: stopping on SIGINT"); Ok(()) }
    };
    let _ = std::fs::remove_file(path);
    result
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
