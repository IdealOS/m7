//! What a long document costs in a text area, per keystroke and per caret move:
//! the desktop handling the key (editing, placing the caret), building the draw
//! list, and the renderer (shaping included). Run with `make text-bench`.
//!
//! The document is prose in paragraphs of a few hundred characters, typed into
//! and moved through from the middle, as when editing a chapter.

use std::time::{Duration, Instant};

use compositor::desktop::{Call, Desktop, Out};
use compositor::headless::Offscreen;
use compositor::input::{Input, Key, Mods};
use compositor::scene::Rgba;
use protocol::{NodeKind, Sender};
use serde_json::{Value, json};

const W: f32 = 1280.0;
const H: f32 = 800.0;
const STEPS: usize = 40;

/// About `bytes` of prose: paragraphs of 40 to 80 words from a small vocabulary.
fn document(bytes: usize) -> String {
    const WORDS: &[&str] = &[
        "the",
        "light",
        "over",
        "harbour",
        "was",
        "thin",
        "and",
        "grey",
        "when",
        "she",
        "walked",
        "down",
        "to",
        "boats",
        "a",
        "gull",
        "cried",
        "somewhere",
        "beyond",
        "breakwater",
        "nobody",
        "answered",
        "morning",
        "smelled",
        "of",
        "salt",
        "tar",
        "rope",
        "wet",
        "stone",
        "remembered",
        "letter",
        "folded",
        "in",
        "her",
        "coat",
        "pocket",
        "unread",
        "since",
        "Tuesday",
    ];
    let mut seed = 0x2545_f491_u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let mut out = String::new();
    while out.len() < bytes {
        let n = 40 + next() % 40;
        for i in 0..n {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(WORDS[next() as usize % WORDS.len()]);
        }
        out.push_str(".\n\n");
    }
    out
}

struct Timed {
    desktop: Duration,
    draw_list: Duration,
    render: Duration,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Apply `input` `STEPS` times, timing each part of a frame, and report the mean.
fn run(d: &mut Desktop, gpu: &mut Option<Offscreen>, input: Input) -> Timed {
    let mut t = Timed { desktop: Duration::ZERO, draw_list: Duration::ZERO, render: Duration::ZERO };
    for _ in 0..STEPS {
        let started = Instant::now();
        d.input(input);
        t.desktop += started.elapsed();
        let started = Instant::now();
        let list = d.draw_list();
        t.draw_list += started.elapsed();
        if let Some(gpu) = gpu.as_mut() {
            gpu.render(&list, 1.0, Rgba::TRANSPARENT, None, |_| {}).expect("renders");
            t.render += gpu.timing.encode;
        }
    }
    let n = STEPS as u32;
    Timed { desktop: t.desktop / n, draw_list: t.draw_list / n, render: t.render / n }
}

fn main() {
    let mut gpu = match Offscreen::new(W as u32, H as u32, wgpu::TextureFormat::Rgba8UnormSrgb) {
        Ok(o) => {
            println!("renderer: {} ({:?})", o.adapter.name, o.adapter.backend);
            Some(o)
        }
        Err(e) => {
            println!("renderer: none ({e}); timing the desktop only");
            None
        }
    };
    println!(
        "{:>9} {:>9}  {:<12} {:>9} {:>9} {:>9} {:>9}",
        "bytes", "paras", "step", "desktop", "draw", "render", "total"
    );
    for bytes in [2_000, 20_000, 200_000] {
        let text = document(bytes);
        let mut d = Desktop::with_fonts(W, H, compositor::fonts::ui_only());
        d.scene.apply_all(compositor::demo::build(W, H, 0));
        d.set_screen(W, H);
        let app = Some(Sender::App { app: "org.idealos.bench".into(), conn: 1 });
        let out = d.call(Call::Request {
            id: 1,
            from: app.clone(),
            method: "open_window".into(),
            params: json!({"title": "Bench", "width": 900, "height": 640}),
            stream: true,
        });
        let window = out
            .iter()
            .find_map(|o| match o {
                Out::Part { data, .. } => data.get("window").and_then(Value::as_u64),
                _ => None,
            })
            .expect("a window");
        let ops = json!([
            {"op": "create", "id": 2, "type": "text_area", "props": {"value": text, "w": 860, "h": 580}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0}
        ]);
        d.call(Call::Request {
            id: 2,
            from: app,
            method: "commit".into(),
            params: json!({"window": window, "ops": ops}),
            stream: false,
        });
        // Find the text area on screen and click into it.
        d.relayout();
        let area = d
            .scene
            .ids()
            .find(|&id| d.scene.node(id).is_some_and(|n| n.kind == NodeKind::TextArea))
            .expect("the text area");
        let (rect, _) = d.scene.screen_rects()[&area];
        d.input(Input::MotionTo { x: rect.x + rect.w / 2.0, y: rect.y + rect.h / 2.0 });
        d.input(Input::Button { pressed: true });
        d.input(Input::Button { pressed: false });
        let started = Instant::now();
        let list = d.draw_list();
        if let Some(gpu) = gpu.as_mut() {
            gpu.render(&list, 1.0, Rgba::TRANSPARENT, None, |_| {}).expect("renders");
        }
        let first = started.elapsed();
        let paras = text.split("\n\n").count();
        println!(
            "{:>9} {:>9}  {:<12} {:>9} {:>9} {:>9} {:>9.2}",
            text.len(),
            paras,
            "first frame",
            "",
            "",
            "",
            ms(first)
        );
        // The first scroll needs the whole text's height.
        let started = Instant::now();
        d.input(Input::Scroll { dx: 0.0, dy: 40.0 });
        let list = d.draw_list();
        if let Some(gpu) = gpu.as_mut() {
            gpu.render(&list, 1.0, Rgba::TRANSPARENT, None, |_| {}).expect("renders");
        }
        println!(
            "{:>9} {:>9}  {:<12} {:>9} {:>9} {:>9} {:>9.2}",
            text.len(),
            paras,
            "first scroll",
            "",
            "",
            "",
            ms(started.elapsed())
        );
        d.input(Input::Scroll { dx: 0.0, dy: -40.0 });
        let plain = Mods::default();
        let before = d.draw_list().layers.iter().flat_map(|l| l.texts.iter()).map(|t| t.text.len()).max();
        for (name, input) in [
            ("type", Input::Key { key: Key::Char('x'), mods: plain }),
            ("caret down", Input::Key { key: Key::Down, mods: plain }),
            ("caret right", Input::Key { key: Key::Right, mods: plain }),
            ("scroll", Input::Scroll { dx: 0.0, dy: 20.0 }),
        ] {
            let t = run(&mut d, &mut gpu, input);
            println!(
                "{:>9} {:>9}  {:<12} {:>9.2} {:>9.2} {:>9.2} {:>9.2}",
                text.len(),
                paras,
                name,
                ms(t.desktop),
                ms(t.draw_list),
                ms(t.render),
                ms(t.desktop + t.draw_list + t.render)
            );
        }
        let after = d.draw_list().layers.iter().flat_map(|l| l.texts.iter()).map(|t| t.text.len()).max();
        assert_eq!(after, before.map(|b| b + STEPS), "the keys typed into the text area");
    }
    println!("(milliseconds, the mean of {STEPS} steps each)");
}
