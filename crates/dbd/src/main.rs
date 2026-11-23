//! `dbd [--db PATH] [--snapshots DIR] [--blobs DIR] [--socket PATH]`: run the `db` service.
//!
//! The database is `/data/db/idealos.db` on the device, and otherwise
//! `$XDG_DATA_HOME/idealos/idealos.db` (or `~/.local/share/idealos/idealos.db`).
//! `IDEALOS_DB` overrides that, and `--db` overrides both. Snapshots and blobs go in
//! `snapshots/` and `blobs/` beside the database unless `--snapshots` or `--blobs`
//! say otherwise.
//!
//! Started by session as a `bus=system` service, it uses the bus connection session
//! made for it; otherwise it connects to the default socket (or `--socket`).

use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let Args { db: db_path, config, socket } = match parse_args(std::env::args().skip(1)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("dbd: {e}\nusage: dbd [--db PATH] [--snapshots DIR] [--blobs DIR] [--socket PATH]");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    if let Err(e) = runtime.block_on(run(db_path, config, socket)) {
        log::error!("dbd: {e}");
        std::process::exit(1);
    }
}

struct Args {
    db: PathBuf,
    config: dbd::Config,
    socket: Option<PathBuf>,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut db = std::env::var_os("IDEALOS_DB").map(PathBuf::from).unwrap_or_else(default_db_path);
    let (mut snapshots, mut blobs, mut socket): (Option<PathBuf>, Option<PathBuf>, _) = (None, None, None);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a path"));
        match arg.as_str() {
            "--db" => db = value()?.into(),
            "--snapshots" => snapshots = Some(value()?.into()),
            "--blobs" => blobs = Some(value()?.into()),
            "--socket" => socket = Some(value()?.into()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let beside = dbd::Config::beside(&db);
    let config = dbd::Config {
        snapshots: snapshots.unwrap_or(beside.snapshots),
        blobs: blobs.unwrap_or(beside.blobs),
    };
    Ok(Args { db, config, socket })
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

async fn run(db_path: PathBuf, config: dbd::Config, socket: Option<PathBuf>) -> Result<(), String> {
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
    log::info!(
        "dbd: serving {} from {} (snapshots in {}, blobs in {})",
        protocol::db::SERVICE,
        db_path.display(),
        config.snapshots.display(),
        config.blobs.display()
    );
    dbd::serve(bus, store, config).await?;
    Err("the bus closed the connection".into())
}
