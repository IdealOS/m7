//! `compositor` binary.
//!
//! ```text
//! compositor                         # hosted window with the demo scene
//! compositor --continuous            # redraw every vsync to measure frame time
//! compositor --windows 40            # demo scene size
//! compositor --screenshot out.png    # render one frame offscreen and exit
//! compositor --screenshot out.png --windows 0 --serve-for 3 --click "Add one"
//!                                    # serve app windows on the bus for 3 s,
//!                                    # pressing the button labelled "Add one"
//!                                    # partway through, then render
//! compositor --backend kms           # device backend (Linux, feature "kms")
//! compositor --backend kms --card /dev/dri/card1
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use compositor::backend::Options;
use compositor::desktop::Desktop;
use compositor::input::Input;
use compositor::{bus, demo, headless};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut opts = Options::default();
    let mut backend = String::from("hosted");
    let mut screenshot: Option<PathBuf> = None;
    let mut scale = 2.0_f32;
    let mut serve_for = 0.0_f64;
    let mut clicks: Vec<String> = Vec::new();
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
            "--card" => opts.card = Some(value("--card")),
            "--screenshot" => screenshot = Some(value("--screenshot").into()),
            "--serve-for" => {
                serve_for = value("--serve-for").parse().unwrap_or_else(|_| usage("bad --serve-for"))
            }
            "--click" => clicks.push(value("--click")),
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }

    let result = if let Some(path) = screenshot {
        take_screenshot(&opts, scale, &path, serve_for, &clicks)
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

fn take_screenshot(
    opts: &Options,
    scale: f32,
    path: &Path,
    serve_for: f64,
    clicks: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut desktop = Desktop::new(opts.width as f32, opts.height as f32);
    desktop.scene.apply_all(demo::build(opts.width as f32, opts.height as f32, opts.windows));
    if serve_for > 0.0 {
        serve(&mut desktop, serve_for, clicks)?;
    }
    let scene = &mut desktop.scene;
    let summary = format!("{} nodes · offscreen", scene.len());
    scene.apply(demo::hud(&summary));
    let started = std::time::Instant::now();
    let img = headless::render(scene, opts.width, opts.height, scale)?;
    log::info!("rendered {}x{} in {:?}", img.width, img.height, started.elapsed());
    img.save_png(path)?;
    println!("wrote {}", path.display());
    Ok(())
}

/// Serve app windows on the bus for `secs`, pressing each button in `clicks` (by
/// its label) in turn, spread over the time.
fn serve(desktop: &mut Desktop, secs: f64, clicks: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let bridge = bus::connect(|| {}).ok_or("--serve-for needs a bus")?;
    let start = std::time::Instant::now();
    let total = std::time::Duration::from_secs_f64(secs);
    let mut pending: Vec<(std::time::Duration, &String)> = clicks
        .iter()
        .enumerate()
        .map(|(i, c)| (total.mul_f64((i + 1) as f64 / (clicks.len() + 1) as f64), c))
        .collect();
    pending.reverse();
    while start.elapsed() < total {
        for call in bridge.calls() {
            bridge.send(desktop.call(call));
        }
        if let Some((at, label)) = pending.last()
            && start.elapsed() >= *at
        {
            let list = desktop.scene.draw_list();
            match list.layers.iter().flat_map(|l| &l.texts).find(|t| &t.text == *label) {
                Some(t) => {
                    let (x, y) = (t.rect.x + t.rect.w / 2.0, t.rect.y + t.rect.h / 2.0);
                    for input in [
                        Input::MotionTo { x, y },
                        Input::Button { pressed: true },
                        Input::Button { pressed: false },
                    ] {
                        let (_, outs) = desktop.input(input);
                        bridge.send(outs);
                    }
                    log::info!("clicked {label:?} at ({x}, {y})");
                }
                None => log::warn!("no button labelled {label:?} on screen"),
            }
            pending.pop();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Ok(())
}

fn usage(err: &str) -> ! {
    if !err.is_empty() {
        eprintln!("error: {err}\n");
    }
    eprintln!(
        "usage: compositor [--backend hosted|kms] [--card /dev/dri/cardN] [--windows N] [--size WxH]\n                  [--continuous] [--screenshot out.png [--scale S] [--serve-for SECS] [--click LABEL]...]"
    );
    std::process::exit(if err.is_empty() { 0 } else { 2 })
}
