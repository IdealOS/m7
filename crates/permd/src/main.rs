//! `permd [--db PATH] [--socket PATH]`: run the `perm` service.
//!
//! Grants live in `/data/perm/grants.db` on the device, and otherwise in
//! `$XDG_DATA_HOME/idealos/grants.db` (or `~/.local/share/idealos/grants.db`).
//! `IDEALOS_PERM_DB` overrides that, and `--db` overrides both.
//!
//! Started by session as a `bus=system` service, it uses the bus connection session
//! made for it; otherwise it connects to the default socket (or `--socket`).

use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let (db, socket) = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("permd: {e}\nusage: permd [--db PATH] [--socket PATH]");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    if let Err(e) = runtime.block_on(run(db, socket)) {
        log::error!("permd: {e}");
        std::process::exit(1);
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<(PathBuf, Option<PathBuf>), String> {
    let mut db = std::env::var_os("IDEALOS_PERM_DB").map(PathBuf::from).unwrap_or_else(default_path);
    let mut socket = None;
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a path"));
        match arg.as_str() {
            "--db" => db = value()?.into(),
            "--socket" => socket = Some(value()?.into()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok((db, socket))
}

fn default_path() -> PathBuf {
    // On the device, /data is the one writable filesystem (the root is read-only).
    if cfg!(target_os = "linux") && std::path::Path::new("/data/etc").is_dir() {
        return "/data/perm/grants.db".into();
    }
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".local/share")
    });
    base.join("idealos/grants.db")
}

async fn run(db: PathBuf, socket: Option<PathBuf>) -> Result<(), String> {
    if let Some(dir) = db.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let store = permd::Store::open(&db).map_err(|e| format!("cannot open {}: {e}", db.display()))?;
    let bus = match &socket {
        Some(path) => bus_client::Client::connect(path).await,
        None => bus_client::Client::connect_default().await,
    }
    .map_err(|e| format!("cannot reach the bus: {e}"))?;
    bus.claim(protocol::perm::SERVICE)
        .await
        .map_err(|e| format!("cannot claim {}: {e}", protocol::perm::SERVICE))?;
    log::info!("permd: serving {} from {}", protocol::perm::SERVICE, db.display());
    permd::serve(bus, store).await?;
    Err("the bus closed the connection".into())
}
