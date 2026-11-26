//! `backupd [--db-dir DIR] [--dir PATH] [--manual] [--socket PATH]`: run the `backup`
//! service.
//!
//! dbd's snapshots and blobs are read from `DIR/snapshots` and `DIR/blobs`
//! (`/data/db` on the device, as dbd keeps them). Drives are USB drives holding
//! exFAT, unless `--dir` makes a directory stand in for one (for development).
//! `--manual` turns off the automatic backups when a backup drive is plugged in.
//!
//! Started by session as a `bus=system` service, it uses the bus connection session
//! made for it; otherwise it connects to the default socket (or `--socket`).

use std::path::PathBuf;

use backupd::drives::Drives;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut socket: Option<PathBuf> = None;
    let mut db_dir = PathBuf::from("/data/db");
    let mut dir: Option<PathBuf> = None;
    let mut automatic = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().map(PathBuf::from);
        match arg.as_str() {
            "--socket" => socket = value(),
            "--db-dir" => db_dir = value().unwrap_or(db_dir),
            "--dir" => dir = value(),
            "--manual" => automatic = false,
            other => {
                eprintln!(
                    "backupd: unknown argument {other}\n\
                     usage: backupd [--db-dir DIR] [--dir PATH] [--manual] [--socket PATH]"
                );
                std::process::exit(2);
            }
        }
    }
    let drives = match dir {
        Some(path) => Drives::Dir(path),
        None => Drives::Block { mounts: "/run/idealos/backup".into() },
    };
    let config = backupd::Config { db_dir, drives, automatic };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let result = runtime.block_on(async move {
        let bus = match &socket {
            Some(path) => bus_client::Client::connect(path).await,
            None => bus_client::Client::connect_default().await,
        }
        .map_err(|e| format!("cannot reach the bus: {e}"))?;
        bus.claim(protocol::backup::SERVICE).await.map_err(|e| format!("cannot claim backup: {e}"))?;
        log::info!("backupd: serving backup (database in {})", config.db_dir.display());
        backupd::serve(bus, config).await?;
        Err::<(), String>("the bus closed the connection".into())
    });
    if let Err(e) = result {
        log::error!("backupd: {e}");
        std::process::exit(1);
    }
}
