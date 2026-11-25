//! `ptyd [--socket PATH] [--shell CMD...]`: run the `pty` service.
//!
//! Sessions run `$SHELL -l` (or `/bin/sh -l`), or the command after `--shell`, in
//! `$HOME` (or `/`). Started by session as a `bus=system` service, it uses the bus
//! connection session made for it; otherwise it connects to the default socket
//! (or `--socket`).

use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut socket: Option<PathBuf> = None;
    let mut shell: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from),
            "--shell" => {
                shell = args.by_ref().collect();
                break;
            }
            other => {
                eprintln!("ptyd: unknown argument {other}\nusage: ptyd [--socket PATH] [--shell CMD...]");
                std::process::exit(2);
            }
        }
    }
    if shell.is_empty() {
        shell = vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()), "-l".into()];
    }
    let dir =
        std::env::var_os("HOME").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(|| "/".into());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let result = runtime.block_on(async move {
        let bus = match &socket {
            Some(path) => bus_client::Client::connect(path).await,
            None => bus_client::Client::connect_default().await,
        }
        .map_err(|e| format!("cannot reach the bus: {e}"))?;
        bus.claim(protocol::pty::SERVICE).await.map_err(|e| format!("cannot claim pty: {e}"))?;
        log::info!("ptyd: serving pty, running {}", shell.join(" "));
        ptyd::serve(bus, ptyd::Config { shell, dir }).await?;
        Err::<(), String>("the bus closed the connection".into())
    });
    if let Err(e) = result {
        log::error!("ptyd: {e}");
        std::process::exit(1);
    }
}
