//! `app-host`: run one app, or install the apps that ship with the system.
//!
//! ```text
//! app-host --app ID        run installed app ID (what session's `launch` starts)
//! app-host --bundle FILE   run a bundle file directly (development)
//! app-host --install DIR   install each DIR/*/{manifest.json,app.js}, launching
//!                          the ones marked autostart; then exit
//! ```
//!
//! It uses the bus connection session made for it (which identifies an app by its
//! id) or, run by hand, the default socket.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use app_host::install::{self, Manifest};
use app_host::runtime::{self, Exit};
use bus_client::Client;

enum Mode {
    App(String),
    Bundle(PathBuf),
    Install(PathBuf),
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mode = match parse(std::env::args().skip(1)) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("app-host: {e}\nusage: app-host (--app ID | --bundle FILE | --install DIR)");
            return ExitCode::from(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let local = tokio::task::LocalSet::new();
    match local.block_on(&runtime, run(mode)) {
        Ok(code) => code,
        Err(e) => {
            log::error!("app-host: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Mode, String> {
    let (flag, value) = (args.next().ok_or("missing a mode")?, args.next());
    let value = value.ok_or(format!("{flag} needs a value"))?;
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument {extra}"));
    }
    match flag.as_str() {
        "--app" => Ok(Mode::App(value)),
        "--bundle" => Ok(Mode::Bundle(value.into())),
        "--install" => Ok(Mode::Install(value.into())),
        other => Err(format!("unknown argument {other}")),
    }
}

async fn run(mode: Mode) -> Result<ExitCode, String> {
    let bus = Client::connect_default().await.map_err(|e| format!("cannot reach the bus: {e}"))?;
    let (name, source) = match mode {
        Mode::Install(dir) => return install_all(&bus, &dir).await.map(|()| ExitCode::SUCCESS),
        Mode::Bundle(path) => {
            let source = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            (path.display().to_string(), source)
        }
        Mode::App(id) => {
            let source = install::fetch_bundle(&bus, &id).await.map_err(|e| e.to_string())?;
            (format!("{id}/app.js"), source)
        }
    };
    log::info!("app-host: running {name}");
    Ok(match runtime::run(bus, &name, &source).await? {
        Exit::Code(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Exit::Idle => ExitCode::SUCCESS,
        Exit::Failed(e) => {
            log::error!("app-host: {name} failed to load: {e}");
            ExitCode::FAILURE
        }
    })
}

/// Wait (up to 30 s) until `names` are all on the bus: at boot, session starts
/// services together, and installing needs the database, launching needs session,
/// and apps need the compositor for their windows.
async fn wait_for(bus: &Client, names: &[&str]) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let missing: Vec<&str> = match bus.list().await {
            Ok(list) => names.iter().copied().filter(|n| !list.services.contains_key(*n)).collect(),
            Err(_) => names.to_vec(),
        };
        if missing.is_empty() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            log::warn!("app-host: going ahead without {}", missing.join(", "));
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Install every app under `dir`, and launch the autostart ones. One app's failure
/// doesn't stop the others.
async fn install_all(bus: &Client, dir: &Path) -> Result<(), String> {
    wait_for(
        bus,
        &[
            protocol::db::SERVICE,
            protocol::perm::SERVICE,
            protocol::session::SERVICE,
            protocol::scene::SERVICE,
        ],
    )
    .await;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("manifest.json").is_file() && p.join("app.js").is_file())
        .collect();
    entries.sort();
    for app in entries {
        let manifest = match Manifest::read(&app.join("manifest.json")) {
            Ok(m) => m,
            Err(e) => {
                log::error!("app-host: {e}");
                continue;
            }
        };
        match install::install(bus, &manifest, &app.join("app.js")).await {
            Ok(done) => log::info!(
                "app-host: {} {}",
                manifest.id,
                if done.changed { "installed" } else { "already installed" }
            ),
            Err(e) => {
                log::error!("app-host: installing {}: {e}", manifest.id);
                continue;
            }
        }
        match install::grant(bus, &manifest).await {
            Ok(made) => {
                for g in made {
                    log::info!("app-host: granted {} to {}", g.rule.describe(), manifest.id);
                }
            }
            // It runs, with what it has; it can still ask.
            Err(e) => log::error!("app-host: granting {} its permissions: {e}", manifest.id),
        }
        if manifest.autostart
            && let Err(e) = install::launch(bus, &manifest.id).await
        {
            log::error!("app-host: launching {}: {e}", manifest.id);
        }
    }
    Ok(())
}
