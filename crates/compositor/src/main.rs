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
//!                                    # and --key NAME (tab, enter, down, cmd+z,
//!                                    # alt+up, ...) are
//!                                    # steps too, spread over the time in order;
//!                                    # --press LABEL, --move DY and --release
//!                                    # drag (a playlist's row, say)
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
use compositor::input::{Input, Key, MediaKey, Mods};
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
            "--press" => steps.push(Step::Press(value("--press"))),
            "--move" => {
                steps.push(Step::Move(value("--move").parse().unwrap_or_else(|_| usage("bad --move"))))
            }
            "--release" => steps.push(Step::Release),
            "--type" => steps.push(Step::Type(value("--type"))),
            "--scroll" => {
                steps.push(Step::Scroll(value("--scroll").parse().unwrap_or_else(|_| usage("bad --scroll"))))
            }
            "--key" => {
                let name = value("--key");
                if let Some(media) = media_key_named(&name) {
                    steps.push(Step::Media(media));
                } else {
                    let (key, mods) =
                        key_named(&name).unwrap_or_else(|| usage(&format!("unknown key {name}")));
                    steps.push(Step::Key(key, mods))
                }
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
    /// Hold the button down on the text that's this, to drag it.
    Press(String),
    /// Move the pointer this many pixels down (up if negative), holding what's held.
    Move(f32),
    /// Let go of what's held.
    Release,
    /// Type this text into whatever has focus.
    Type(String),
    /// A key, with modifiers.
    Key(Key, Mods),
    /// Turn the wheel under the pointer this many pixels.
    Scroll(f32),
    /// A media key.
    Media(MediaKey),
}

/// A media key by name: `play-pause`, `next`, `volume-up` and the like.
fn media_key_named(name: &str) -> Option<MediaKey> {
    Some(match name {
        "play-pause" => MediaKey::PlayPause,
        "pause" => MediaKey::Pause,
        "stop" => MediaKey::Stop,
        "next" => MediaKey::Next,
        "previous" => MediaKey::Previous,
        "volume-up" => MediaKey::VolumeUp,
        "volume-down" => MediaKey::VolumeDown,
        "mute" => MediaKey::Mute,
        _ => return None,
    })
}

/// A key by name, after any modifiers: `enter`, `cmd+z`, `alt+shift+up`. A
/// single character is that character's key.
fn key_named(name: &str) -> Option<(Key, Mods)> {
    let mut mods = Mods::default();
    let mut parts: Vec<&str> = name.split('+').collect();
    let last = parts.pop()?;
    for m in parts {
        match m {
            "cmd" | "logo" => mods.logo = true,
            "ctrl" => mods.ctrl = true,
            "alt" | "option" => mods.alt = true,
            "shift" => mods.shift = true,
            _ => return None,
        }
    }
    let mut chars = last.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next()) {
        return Some((Key::Char(if mods.shift { c.to_ascii_uppercase() } else { c }), mods));
    }
    Some((named_key(last)?, mods))
}

fn named_key(name: &str) -> Option<Key> {
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
                Step::Click(label) | Step::Press(label) => {
                    let list = desktop.draw_list();
                    match list.layers.iter().flat_map(|l| &l.texts).find(|t| &t.text == label) {
                        Some(t) => {
                            let (x, y) = (t.rect.x + t.rect.w / 2.0, t.rect.y + t.rect.h / 2.0);
                            log::info!("pressing {label:?} at ({x}, {y})");
                            let mut inputs = vec![Input::MotionTo { x, y }, Input::Button { pressed: true }];
                            if matches!(step, Step::Click(_)) {
                                inputs.push(Input::Button { pressed: false });
                            }
                            inputs
                        }
                        None => {
                            log::warn!("no {label:?} on screen");
                            Vec::new()
                        }
                    }
                }
                Step::Move(dy) => {
                    // In small steps, as a hand would.
                    let (x, y) = desktop.seat.cursor();
                    (1..=8).map(|i| Input::MotionTo { x, y: y + dy * i as f32 / 8.0 }).collect()
                }
                Step::Release => vec![Input::Button { pressed: false }],
                Step::Type(text) => {
                    text.chars().map(|c| Input::Key { key: Key::Char(c), mods: Mods::default() }).collect()
                }
                Step::Key(key, mods) => vec![Input::Key { key: *key, mods: *mods }],
                Step::Scroll(dy) => vec![Input::Scroll { dx: 0.0, dy: *dy }],
                Step::Media(key) => vec![Input::Media(*key)],
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
        "usage: compositor [--backend hosted|kms] [--card /dev/dri/cardN] [--render-height auto|native|ROWS] [--scanout auto|copy|direct] [--theme light|dark] [--windows N] [--size WxH]\n                  [--continuous] [--screenshot out.png [--scale S] [--serve-for SECS] [--click LABEL | --press LABEL | --move DY | --release | --type TEXT | --key NAME | --key play-pause|next|previous|volume-up|volume-down|mute]...]"
    );
    std::process::exit(if err.is_empty() { 0 } else { 2 })
}
