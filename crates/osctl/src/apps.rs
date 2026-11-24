//! `osctl app ...`: install, list, launch and stop apps.
//!
//! `install DIR` takes an app's source directory (package.json and src/main.ts),
//! which it builds with `tools/build-app.ts` from the repo it's in, or a built one
//! (manifest.json and app.js). It installs the app, then asks session to launch it.
//! Without session (hosted development on a Mac) it runs `app-host --app ID` itself
//! and stays in the foreground. `--watch` rebuilds, reinstalls and relaunches
//! whenever the app's sources or the SDK change.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime};

use app_host::install::{self, Manifest};
use bus_client::{CallError, Client};
use protocol::session;
use serde_json::json;

/// How often `--watch` looks for changes.
const POLL: Duration = Duration::from_millis(400);

/// Where an app's bundle and manifest are, building it first if `dir` holds sources.
fn prepare(dir: &Path) -> Result<(Manifest, PathBuf), String> {
    let built = if dir.join("manifest.json").is_file() && dir.join("app.js").is_file() {
        dir.to_path_buf()
    } else if dir.join("package.json").is_file() {
        let tool = find_up(dir, "tools/build-app.ts").ok_or_else(|| {
            format!("{} is an app's sources, but no tools/build-app.ts is above it", dir.display())
        })?;
        let status = Command::new("node")
            .arg(&tool)
            .arg(dir)
            .status()
            .map_err(|e| format!("running node {}: {e}", tool.display()))?;
        if !status.success() {
            return Err(format!("building {} failed", dir.display()));
        }
        dir.join("dist")
    } else {
        return Err(format!("{} has neither package.json nor manifest.json and app.js", dir.display()));
    };
    Ok((Manifest::read(&built.join("manifest.json"))?, built.join("app.js")))
}

/// `name` in `dir` or the nearest directory above it.
fn find_up(dir: &Path, name: &str) -> Option<PathBuf> {
    let dir = dir.canonicalize().ok()?;
    dir.ancestors().map(|d| d.join(name)).find(|p| p.exists())
}

/// Files whose change means a rebuild: the app's own and, in a repo, the SDK's.
fn watched(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut roots = vec![dir.to_path_buf()];
    if let Some(sdk) = find_up(dir, "sdk/src") {
        roots.push(sdk);
    }
    let mut out = Vec::new();
    while let Some(d) = roots.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name();
            if name == "dist" || name == "node_modules" || name.to_string_lossy().starts_with('.') {
                continue;
            }
            if path.is_dir() {
                roots.push(path);
            } else if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                out.push((path, m));
            }
        }
    }
    out.sort();
    out
}

/// A locally run app-host, when there's no session to launch apps.
struct Local {
    child: Option<Child>,
}

impl Local {
    fn restart(&mut self, id: &str, out: &mut impl Write) -> Result<(), String> {
        self.stop();
        // Next to osctl (target/debug, or /usr/bin on the device), else on PATH.
        let host = std::env::current_exe()
            .ok()
            .map(|p| p.with_file_name("app-host"))
            .filter(|p| p.exists())
            .unwrap_or_else(|| "app-host".into());
        let child = Command::new(&host)
            .args(["--app", id])
            .spawn()
            .map_err(|e| format!("running {}: {e}", host.display()))?;
        let _ = writeln!(out, "no session on the bus; running {id} here (pid {})", child.id());
        self.child = Some(child);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Install (and launch) the app in `dir`; with `watch`, keep doing it on changes.
pub async fn install(bus: &Client, dir: &Path, watch: bool, out: &mut impl Write) -> Result<(), String> {
    let mut local = Local { child: None };
    let mut seen = watched(dir);
    loop {
        match install_once(bus, dir, &mut local, out).await {
            Ok(()) => {}
            Err(e) if watch => {
                let _ = writeln!(out, "osctl: {e}");
            }
            Err(e) => return Err(e),
        }
        out.flush().map_err(|e| e.to_string())?;
        if !watch {
            // Running the app here means staying until it ends.
            if let Some(child) = local.child.as_mut() {
                let _ = child.wait();
            }
            return Ok(());
        }
        loop {
            tokio::time::sleep(POLL).await;
            let now = watched(dir);
            if now != seen {
                seen = now;
                break;
            }
            if let Some(c) = local.child.as_mut()
                && let Ok(Some(status)) = c.try_wait()
            {
                let _ = writeln!(out, "the app exited ({status}); waiting for changes");
                local.child = None;
            }
        }
    }
}

async fn install_once(
    bus: &Client,
    dir: &Path,
    local: &mut Local,
    out: &mut impl Write,
) -> Result<(), String> {
    let (manifest, bundle) = prepare(dir)?;
    let done = install::install(bus, &manifest, &bundle).await.map_err(|e| e.to_string())?;
    let _ = writeln!(
        out,
        "{} {} (blob {})",
        manifest.id,
        if done.changed { "installed" } else { "unchanged" },
        &done.hash[..12]
    );
    match install::grant(bus, &manifest).await {
        Ok(made) => {
            for g in made {
                let _ = writeln!(out, "granted {}: {}", manifest.id, g.rule.describe());
            }
        }
        Err(CallError::Bus(e)) if e.code == protocol::bus::code::NO_SUCH_SERVICE => {
            let _ = writeln!(out, "permd isn't running: {} gets no permissions", manifest.id);
        }
        Err(e) => return Err(format!("granting {} its permissions: {e}", manifest.id)),
    }
    match install::launch(bus, &manifest.id).await {
        Ok(()) => {
            let _ = writeln!(out, "session launched {}", manifest.id);
            Ok(())
        }
        Err(CallError::Bus(e)) if e.code == protocol::bus::code::NO_SUCH_SERVICE => {
            local.restart(&manifest.id, out)
        }
        Err(e) => Err(format!("launching {}: {e}", manifest.id)),
    }
}

/// Installed apps, and which session is running.
pub async fn list(bus: &Client, out: &mut impl Write) -> Result<(), String> {
    let docs = install::installed(bus).await.map_err(|e| e.to_string())?;
    let running: Vec<String> = match bus.request(session::SERVICE, session::method::APPS, json!(null)).await {
        Ok(v) => serde_json::from_value::<session::AppsResult>(v).map(|r| r.running).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    for d in docs {
        let id = d.data.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let name = d.data.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let version = d.data.get("version").and_then(|v| v.as_str()).unwrap_or("-");
        let state = if running.iter().any(|r| r == id) { "running" } else { "installed" };
        writeln!(out, "{id:<28} {name:<16} {version:<8} {state}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Ask session to launch or stop app `id`.
pub async fn control(bus: &Client, method: &str, id: &str) -> Result<(), String> {
    bus.request(session::SERVICE, method, json!({"app": id})).await.map(drop).map_err(|e| e.to_string())
}
