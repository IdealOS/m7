//! `dbd [--db PATH] [--socket PATH]`: run the `db` service.
//!
//! The database is `/data/db/idealos.db` on the device, and otherwise
//! `$XDG_DATA_HOME/idealos/idealos.db` (or `~/.local/share/idealos/idealos.db`).
//! `IDEALOS_DB` overrides that, and `--db` overrides both.
//!
//! Started by session as a `bus=system` service, it uses the bus connection session
//! made for it; otherwise it connects to the default socket (or `--socket`).

use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let (db_path, socket) = match parse_args(std::env::args().skip(1)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("dbd: {e}\nusage: dbd [--db PATH] [--socket PATH]");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    if let Err(e) = runtime.block_on(run(db_path, socket)) {
        log::error!("dbd: {e}");
        std::process::exit(1);
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<(PathBuf, Option<PathBuf>), String> {
    let mut db = std::env::var_os("IDEALOS_DB").map(PathBuf::from).unwrap_or_else(default_db_path);
    let mut socket = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => db = args.next().ok_or("--db needs a path")?.into(),
            "--socket" => socket = Some(args.next().ok_or("--socket needs a path")?.into()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok((db, socket))
}

fn default_db_path() -> PathBuf {
    // On the device, /data is the one writable filesystem (the root is read-only).
    if cfg!(target_os = "linux") && std::path::Path::new("/data/etc").is_dir() {
        return "/data/db/idealos.db".into();
    }
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".local/share")
    });
    base.join("idealos/idealos.db")
}

async fn run(db_path: PathBuf, socket: Option<PathBuf>) -> Result<(), String> {
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let store = db::Store::open(&db_path).map_err(|e| format!("cannot open {}: {e}", db_path.display()))?;
    let bus = match &socket {
        Some(path) => bus_client::Client::connect(path).await,
        None => bus_client::Client::connect_default().await,
    }
    .map_err(|e| format!("cannot reach the bus: {e}"))?;
    bus.claim(protocol::db::SERVICE)
        .await
        .map_err(|e| format!("cannot claim {}: {e}", protocol::db::SERVICE))?;
    log::info!("dbd: serving {} from {}", protocol::db::SERVICE, db_path.display());
    dbd::serve(bus, store).await;
    Err("the bus closed the connection".into())
}
