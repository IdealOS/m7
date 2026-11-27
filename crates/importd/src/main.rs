//! `importd [--dir ROOT] [--mounts DIR] [--socket PATH]`: run the `import` service.
//!
//! Shares are mounted under `DIR` while in use (`/run/idealos/import`). `--dir`
//! lets `dir:` sources import from folders inside `ROOT` on this machine, for
//! development; without it, only shares can be imported from.
//!
//! Started by session as a `bus=system` service, it uses the bus connection session
//! made for it; otherwise it connects to the default socket (or `--socket`).

use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut socket: Option<PathBuf> = None;
    let mut config = importd::Config { mounts: "/run/idealos/import".into(), dir_root: None };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().map(PathBuf::from);
        match arg.as_str() {
            "--socket" => socket = value(),
            "--dir" => config.dir_root = value(),
            "--mounts" => config.mounts = value().unwrap_or(config.mounts),
            other => {
                eprintln!(
                    "importd: unknown argument {other}\nusage: importd [--dir ROOT] [--mounts DIR] [--socket PATH]"
                );
                std::process::exit(2);
            }
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let result = runtime.block_on(async move {
        let bus = match &socket {
            Some(path) => bus_client::Client::connect(path).await,
            None => bus_client::Client::connect_default().await,
        }
        .map_err(|e| format!("cannot reach the bus: {e}"))?;
        bus.claim(protocol::import::SERVICE).await.map_err(|e| format!("cannot claim import: {e}"))?;
        log::info!("importd: serving import");
        importd::serve(bus, config).await?;
        Err::<(), String>("the bus closed the connection".into())
    });
    if let Err(e) = result {
        log::error!("importd: {e}");
        std::process::exit(1);
    }
}
