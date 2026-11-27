//! `audiod [--device NAME] [--silent] [--volume V] [--socket PATH]`: run the
//! `audio` service, playing to the default sound device (or the one whose name
//! contains NAME). With no sound device, or `--silent`, it plays to nowhere,
//! keeping time, so everything else still works.

use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut socket: Option<PathBuf> = None;
    let mut config = audiod::Config::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from),
            "--device" => config.device = args.next(),
            "--silent" => config.silent = true,
            "--volume" => match args.next().and_then(|v| v.parse::<f32>().ok()) {
                Some(v) => config.volume = v,
                None => usage("--volume takes a number from 0 to 1"),
            },
            other => usage(&format!("unknown argument {other}")),
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let result = runtime.block_on(async move {
        let bus = match &socket {
            Some(path) => bus_client::Client::connect(path).await,
            None => bus_client::Client::connect_default().await,
        }
        .map_err(|e| format!("cannot reach the bus: {e}"))?;
        bus.claim(protocol::audio::SERVICE).await.map_err(|e| format!("cannot claim audio: {e}"))?;
        let engine = audiod::Engine::start(&config);
        log::info!("audiod: serving audio");
        audiod::serve(bus, engine).await?;
        Err::<(), String>("the bus closed the connection".into())
    });
    if let Err(e) = result {
        log::error!("audiod: {e}");
        std::process::exit(1);
    }
}

fn usage(problem: &str) -> ! {
    eprintln!("audiod: {problem}\nusage: audiod [--device NAME] [--silent] [--volume V] [--socket PATH]");
    std::process::exit(2);
}
