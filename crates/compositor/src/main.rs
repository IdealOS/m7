//! `compositor` binary.
//!
//! ```text
//! compositor                         # hosted window with the demo scene
//! compositor --continuous            # redraw every vsync to measure frame time
//! compositor --windows 40            # demo scene size
//! compositor --screenshot out.png    # render one frame offscreen and exit
//! compositor --backend kms           # device backend (Linux, feature "kms")
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use compositor::backend::Options;
use compositor::{demo, headless, scene::Scene};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut opts = Options::default();
    let mut backend = String::from("hosted");
    let mut screenshot: Option<PathBuf> = None;
    let mut scale = 2.0_f32;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().unwrap_or_else(|| usage(&format!("{name} needs a value")));
        match arg.as_str() {
            "--backend" => backend = value("--backend"),
            "--windows" => {
                opts.windows = value("--windows").parse().unwrap_or_else(|_| usage("bad --windows"))
            }
            "--size" => {
                let v = value("--size");
                let (w, h) = v.split_once('x').unwrap_or_else(|| usage("--size is WxH"));
                opts.width = w.parse().unwrap_or_else(|_| usage("bad --size"));
                opts.height = h.parse().unwrap_or_else(|_| usage("bad --size"));
            }
            "--scale" => scale = value("--scale").parse().unwrap_or_else(|_| usage("bad --scale")),
            "--continuous" => opts.continuous = true,
            "--screenshot" => screenshot = Some(value("--screenshot").into()),
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }

    let result = if let Some(path) = screenshot {
        take_screenshot(&opts, scale, &path)
    } else {
        match backend.as_str() {
            #[cfg(feature = "hosted")]
            "hosted" => compositor::backend::hosted::run(opts),
            #[cfg(feature = "kms")]
            "kms" => compositor::backend::kms::run(opts),
            other => Err(format!("backend {other:?} is not compiled in").into()),
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("compositor: {e}");
            ExitCode::FAILURE
        }
    }
}

fn take_screenshot(opts: &Options, scale: f32, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut scene = Scene::new();
    scene.apply_all(demo::build(opts.width as f32, opts.height as f32, opts.windows));
    let summary = format!("{} nodes · offscreen", scene.len());
    scene.apply(demo::hud(&summary));
    let started = std::time::Instant::now();
    let img = headless::render(&scene, opts.width, opts.height, scale)?;
    log::info!("rendered {}x{} in {:?}", img.width, img.height, started.elapsed());
    img.save_png(path)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn usage(err: &str) -> ! {
    if !err.is_empty() {
        eprintln!("error: {err}\n");
    }
    eprintln!(
        "usage: compositor [--backend hosted|kms] [--windows N] [--size WxH] [--continuous]\n                  [--screenshot out.png [--scale S]]"
    );
    std::process::exit(if err.is_empty() { 0 } else { 2 })
}
