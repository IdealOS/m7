//! `secretsd [--dir DIR] [--socket PATH]`: run the `secrets` service.
//!
//! Secrets live in `secrets.db`, sealed with the key in `key`, both in
//! `/data/secrets` on the device, and otherwise in `$XDG_DATA_HOME/idealos/secrets`
//! (or `~/.local/share/idealos/secrets`). `IDEALOS_SECRETS_DIR` overrides that, and
//! `--dir` overrides both. The directory is made readable by its owner only.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let (dir, socket) = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("secretsd: {e}\nusage: secretsd [--dir DIR] [--socket PATH]");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    if let Err(e) = runtime.block_on(run(dir, socket)) {
        log::error!("secretsd: {e}");
        std::process::exit(1);
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<(PathBuf, Option<PathBuf>), String> {
    let mut dir = std::env::var_os("IDEALOS_SECRETS_DIR").map(PathBuf::from).unwrap_or_else(default_dir);
    let mut socket = None;
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a path"));
        match arg.as_str() {
            "--dir" => dir = value()?.into(),
            "--socket" => socket = Some(value()?.into()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok((dir, socket))
}

fn default_dir() -> PathBuf {
    if cfg!(target_os = "linux") && std::path::Path::new("/data/etc").is_dir() {
        return "/data/secrets".into();
    }
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".local/share")
    });
    base.join("idealos/secrets")
}

async fn run(dir: PathBuf, socket: Option<PathBuf>) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // Made before, perhaps by someone else: keep it private anyway.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let key = secretsd::store::load_key(&dir.join("key")).map_err(|e| e.to_string())?;
    let store = secretsd::Store::open(&dir.join("secrets.db"), &key).map_err(|e| e.to_string())?;
    let bus = match &socket {
        Some(path) => bus_client::Client::connect(path).await,
        None => bus_client::Client::connect_default().await,
    }
    .map_err(|e| format!("cannot reach the bus: {e}"))?;
    bus.claim(protocol::secrets::SERVICE)
        .await
        .map_err(|e| format!("cannot claim {}: {e}", protocol::secrets::SERVICE))?;
    log::info!("secretsd: serving {} from {}", protocol::secrets::SERVICE, dir.display());
    secretsd::serve(bus, store).await?;
    Err("the bus closed the connection".into())
}
