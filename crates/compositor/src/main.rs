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
//!                                    # partway through, then render; --type TEXT
//!                                    # and --key NAME (tab, enter, down, ...) are
//!                                    # steps too, spread over the time in order
//! compositor --backend kms           # device backend (Linux, feature "kms")
//! compositor --backend kms --card /dev/dri/card1
//! compositor --backend kms --scanout copy
//!                                    # how frames reach the display: "direct" renders
//!                                    # into the display's buffers (no copy), "copy"
//!                                    # reads them back and copies; "auto" (the
//!                                    # default) is direct where the GPU can
//! compositor --backend kms --render-height 720
//!                                    # render at most 720 rows and let the display
//!                                    # scale them up (text gets fuzzy); "auto"
//!                                    # means 720 when rendering on the CPU, and
//!                                    # "native" (the default) never scales
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use compositor::backend::{Options, RenderHeight, Scanout};
use compositor::desktop::Desktop;
use compositor::input::{Input, Key, Mods};
use compositor::{bus, demo, headless};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut opts = Options::default();
    let mut backend = String::from("hosted");
    let mut screenshot: Option<PathBuf> = None;
    let mut scale = 2.0_f32;
    let mut serve_for = 0.0_f64;
    let mut steps: Vec<Step> = Vec::new();
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
            "--scanout" => {
                opts.scanout = Scanout::parse(&value("--scanout"))
                    .unwrap_or_else(|| usage("--scanout is auto, copy or direct"))
            }
            "--render-height" => {
                opts.render_height = RenderHeight::parse(&value("--render-height"))
                    .unwrap_or_else(|| usage("--render-height is auto, native or a number of rows"))
            }
            "--theme" => {
                opts.dark = match value("--theme").as_str() {
                    "dark" => true,
                    "light" => false,
                    _ => usage("--theme is light or dark"),
                }
            }
            "--screenshot" => screenshot = Some(value("--screenshot").into()),
            "--serve-for" => {
                serve_for = value("--serve-for").parse().unwrap_or_else(|_| usage("bad --serve-for"))
            }
            "--click" => steps.push(Step::Click(value("--click"))),
            "--type" => steps.push(Step::Type(value("--type"))),
            "--scroll" => {
                steps.push(Step::Scroll(value("--scroll").parse().unwrap_or_else(|_| usage("bad --scroll"))))
            }
            "--key" => {
                let name = value("--key");
                steps.push(Step::Key(
                    key_named(&name).unwrap_or_else(|| usage(&format!("unknown key {name}"))),
                ))
            }
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }

    let result = if let Some(path) = screenshot {
        take_screenshot(&opts, scale, &path, serve_for, &steps)
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

/// One thing the screenshot mode does while it serves the bus.
enum Step {
    /// Press the button (or anything) whose text is this.
    Click(String),
    /// Type this text into whatever has focus.
    Type(String),
    Key(Key),
    /// Turn the wheel under the pointer this many pixels.
    Scroll(f32),
}

fn key_named(name: &str) -> Option<Key> {
    Some(match name {
        "tab" => Key::Tab,
        "enter" => Key::Enter,
        "escape" => Key::Escape,
        "backspace" => Key::Backspace,
        "left" => Key::Left,
        "right" => Key::Right,
        "up" => Key::Up,
        "down" => Key::Down,
        "space" => Key::Char(' '),
        "end" => Key::End,
        _ => return None,
    })
}

fn take_screenshot(
    opts: &Options,
    scale: f32,
    path: &Path,
    serve_for: f64,
    steps: &[Step],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut desktop = Desktop::new(opts.width as f32, opts.height as f32);
    desktop.scene.set_dark(opts.dark);
    desktop.scene.apply_all(demo::build(opts.width as f32, opts.height as f32, opts.windows));
    desktop.set_screen(opts.width as f32, opts.height as f32);
    if serve_for > 0.0 {
        serve(&mut desktop, serve_for, steps)?;
    }
    let summary = format!("{} nodes · offscreen", desktop.scene.len());
    desktop.scene.apply(demo::hud(&summary));
    desktop.relayout();
    let started = std::time::Instant::now();
    let img = headless::render(&desktop.draw_list(), opts.width, opts.height, scale)?;
    log::info!("rendered {}x{} in {:?}", img.width, img.height, started.elapsed());
    img.save_png(path)?;
    println!("wrote {}", path.display());
    Ok(())
}

/// Serve app windows on the bus for `secs`, doing each step in turn, spread over
/// the time.
fn serve(desktop: &mut Desktop, secs: f64, steps: &[Step]) -> Result<(), Box<dyn std::error::Error>> {
    let bridge = bus::connect(|| {}).ok_or("--serve-for needs a bus")?;
    let start = std::time::Instant::now();
    let total = std::time::Duration::from_secs_f64(secs);
    let mut pending: Vec<(std::time::Duration, &Step)> = steps
        .iter()
        .enumerate()
        .map(|(i, c)| (total.mul_f64((i + 1) as f64 / (steps.len() + 1) as f64), c))
        .collect();
    pending.reverse();
    while start.elapsed() < total {
        for call in bridge.calls() {
            bridge.send(desktop.call(call));
        }
        desktop.tick(std::time::Instant::now());
        if let Some((at, step)) = pending.last()
            && start.elapsed() >= *at
        {
            let inputs: Vec<Input> = match step {
                Step::Click(label) => {
                    let list = desktop.draw_list();
                    match list.layers.iter().flat_map(|l| &l.texts).find(|t| &t.text == label) {
                        Some(t) => {
                            let (x, y) = (t.rect.x + t.rect.w / 2.0, t.rect.y + t.rect.h / 2.0);
                            log::info!("clicking {label:?} at ({x}, {y})");
                            vec![
                                Input::MotionTo { x, y },
                                Input::Button { pressed: true },
                                Input::Button { pressed: false },
                            ]
                        }
                        None => {
                            log::warn!("no {label:?} on screen");
                            Vec::new()
                        }
                    }
                }
                Step::Type(text) => {
                    text.chars().map(|c| Input::Key { key: Key::Char(c), mods: Mods::default() }).collect()
                }
                Step::Key(key) => vec![Input::Key { key: *key, mods: Mods::default() }],
                Step::Scroll(dy) => vec![Input::Scroll { dx: 0.0, dy: *dy }],
            };
            for input in inputs {
                let (_, outs) = desktop.input(input);
                bridge.send(outs);
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
        "usage: compositor [--backend hosted|kms] [--card /dev/dri/cardN] [--render-height auto|native|ROWS] [--scanout auto|copy|direct] [--theme light|dark] [--windows N] [--size WxH]\n                  [--continuous] [--screenshot out.png [--scale S] [--serve-for SECS] [--click LABEL | --type TEXT | --key NAME]...]"
    );
    std::process::exit(if err.is_empty() { 0 } else { 2 })
}
