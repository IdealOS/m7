//! `netd [--fake] [--socket PATH]`: run the `net` service over NetworkManager
//! (on the system D-Bus), or, with `--fake`, a pretend network for development.
//! Without NetworkManager it still serves, reporting the network unavailable.

use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut socket: Option<PathBuf> = None;
    let mut fake = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from),
            "--fake" => fake = true,
            other => {
                eprintln!("netd: unknown argument {other}\nusage: netd [--fake] [--socket PATH]");
                std::process::exit(2);
            }
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let result = runtime.block_on(async move {
        let backend = if fake {
            log::info!("netd: using a pretend network (--fake)");
            netd::Backend::Fake(netd::fake::Fake::default())
        } else {
            match netd::nm::Nm::connect().await {
                Ok(nm) => netd::Backend::Nm(nm),
                Err(e) => {
                    log::warn!("netd: {}; reporting the network unavailable", e.message);
                    netd::Backend::None
                }
            }
        };
        let bus = match &socket {
            Some(path) => bus_client::Client::connect(path).await,
            None => bus_client::Client::connect_default().await,
        }
        .map_err(|e| format!("cannot reach the bus: {e}"))?;
        bus.claim(protocol::net::SERVICE).await.map_err(|e| format!("cannot claim net: {e}"))?;
        log::info!("netd: serving net");
        netd::serve(bus, backend).await?;
        Err::<(), String>("the bus closed the connection".into())
    });
    if let Err(e) = result {
        log::error!("netd: {e}");
        std::process::exit(1);
    }
}
