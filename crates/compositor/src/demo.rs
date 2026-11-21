//! The M1 demo scene: a desktop, a top bar and a stack of windows, all built
//! from protocol [`SceneOp`]s exactly as an app would send them over the bus.

use protocol::{NodeId, NodeKind, SceneOp};
use serde_json::{Value, json};

use crate::scene::ROOT;
use crate::wm::TITLE_BAR_HEIGHT;

pub const DESKTOP: NodeId = 1;
pub const TOP_BAR: NodeId = 2;
pub const HUD: NodeId = 3;
const WINDOW_BASE: NodeId = 1000;

pub const BACKGROUND: &str = "#dfe5ec";
const ACCENTS: [&str; 6] = ["#3b82f6", "#10b981", "#f59e0b", "#ef4444", "#8b5cf6", "#06b6d4"];
const APPS: [&str; 8] = ["Documents", "Mail", "Music", "Editor", "Terminal", "Chat", "Settings", "Browser"];

struct Builder {
    ops: Vec<SceneOp>,
}

impl Builder {
    fn node(&mut self, id: NodeId, parent: NodeId, kind: NodeKind, props: Value) {
        self.ops.push(SceneOp::Create {
            id,
            kind,
            props: props.as_object().expect("props are objects").clone(),
        });
        self.ops.push(SceneOp::Insert { parent, child: id, index: usize::MAX });
    }
}

/// Build the demo scene for a screen of `w` by `h` logical pixels.
pub fn build(w: f32, h: f32, windows: usize) -> Vec<SceneOp> {
    let mut b = Builder { ops: Vec::new() };
    b.node(DESKTOP, ROOT, NodeKind::Rect, json!({"w": w, "h": h, "color": BACKGROUND}));

    for i in 0..windows {
        add_window(&mut b, i, w, h);
    }

    // The top bar goes last so it stays above every window.
    b.node(TOP_BAR, ROOT, NodeKind::Group, json!({"w": w, "h": 30}));
    b.node(10, TOP_BAR, NodeKind::Rect, json!({"w": w, "h": 30, "color": "#ffffffe6"}));
    b.node(11, TOP_BAR, NodeKind::Rect, json!({"y": 29, "w": w, "h": 1, "color": "#c7ced8"}));
    b.node(
        12,
        TOP_BAR,
        NodeKind::Text,
        json!({"x": 14, "y": 7, "w": 400, "h": 20, "text": "Phase 0 compositor", "size": 13, "weight": "bold", "color": "#1f2937"}),
    );
    b.node(
        HUD,
        TOP_BAR,
        NodeKind::Text,
        json!({"x": w - 330.0, "y": 7, "w": 316, "h": 20, "text": "", "size": 12, "color": "#4b5563"}),
    );
    b.ops
}

/// Ops that resize the desktop and top bar after the screen size changes.
pub fn resize(w: f32, h: f32) -> Vec<SceneOp> {
    let set = |id: NodeId, props: Value| SceneOp::Set {
        id,
        props: props.as_object().expect("props are objects").clone(),
    };
    vec![
        set(DESKTOP, json!({"w": w, "h": h})),
        set(TOP_BAR, json!({"w": w})),
        set(10, json!({"w": w})),
        set(11, json!({"w": w})),
        set(HUD, json!({"x": w - 330.0})),
    ]
}

/// Ops that update the frame-time readout in the top bar.
pub fn hud(text: &str) -> SceneOp {
    SceneOp::Set { id: HUD, props: json!({"text": text}).as_object().expect("object").clone() }
}

fn add_window(b: &mut Builder, i: usize, screen_w: f32, screen_h: f32) {
    let (ww, wh) = (380.0_f32, 260.0_f32);
    // Cascade in columns of ten so twenty windows fill a laptop-sized screen.
    let col = (i / 10) as f32;
    let row = (i % 10) as f32;
    let x = (48.0 + col * 440.0 + row * 34.0).min((screen_w - ww).max(0.0));
    let y = (52.0 + row * 30.0).min((screen_h - wh).max(30.0));
    let id = WINDOW_BASE + (i as NodeId) * 100;
    let accent = ACCENTS[i % ACCENTS.len()];
    let app = APPS[i % APPS.len()];

    b.node(
        id,
        ROOT,
        NodeKind::Group,
        json!({"window": true, "x": x, "y": y, "w": ww, "h": wh, "clip": true}),
    );
    // Body with a hairline border, then the title bar on top of it.
    b.node(
        id + 1,
        id,
        NodeKind::Rect,
        json!({"w": ww, "h": wh, "radius": 10, "color": "#ffffff", "border_width": 1, "border_color": "#c3cad5"}),
    );
    b.node(
        id + 2,
        id,
        NodeKind::Rect,
        json!({"x": 1, "y": 1, "w": ww - 2.0, "h": TITLE_BAR_HEIGHT - 1.0, "radius": 9, "color": "#f3f5f8"}),
    );
    b.node(
        id + 3,
        id,
        NodeKind::Rect,
        json!({"x": 1, "y": TITLE_BAR_HEIGHT - 12.0, "w": ww - 2.0, "h": 12, "color": "#f3f5f8"}),
    );
    b.node(id + 4, id, NodeKind::Rect, json!({"y": TITLE_BAR_HEIGHT, "w": ww, "h": 1, "color": "#e2e6ec"}));
    for (j, c) in ["#f87171", "#fbbf24", "#34d399"].iter().enumerate() {
        b.node(
            id + 5 + j as NodeId,
            id,
            NodeKind::Rect,
            json!({"x": 14.0 + j as f32 * 18.0, "y": 12, "w": 12, "h": 12, "radius": 6, "color": c}),
        );
    }
    b.node(
        id + 10,
        id,
        NodeKind::Text,
        json!({"x": 74, "y": 9, "w": ww - 90.0, "h": 20, "text": format!("{app} — window {}", i + 1), "size": 13, "weight": "bold", "color": "#1f2937"}),
    );

    // Body: an accent chip, a heading and a few lines of text.
    b.node(
        id + 20,
        id,
        NodeKind::Rect,
        json!({"x": 20, "y": 56, "w": 44, "h": 44, "radius": 10, "color": accent}),
    );
    b.node(
        id + 21,
        id,
        NodeKind::Text,
        json!({"x": 78, "y": 58, "w": ww - 98.0, "h": 22, "text": app, "size": 17, "weight": "bold", "color": "#111827"}),
    );
    b.node(
        id + 22,
        id,
        NodeKind::Text,
        json!({"x": 78, "y": 82, "w": ww - 98.0, "h": 18, "text": "Every record lives in the database.", "size": 12, "color": "#6b7280"}),
    );
    let lines = [
        "Drag a window by its title bar.",
        "Click anywhere in a window to raise it.",
        "Rects are SDF quads; text is shaped by cosmic-text.",
        "Nothing redraws while the scene is idle.",
    ];
    for (j, line) in lines.iter().enumerate() {
        b.node(
            id + 30 + j as NodeId,
            id,
            NodeKind::Text,
            json!({"x": 20, "y": 122.0 + j as f32 * 26.0, "w": ww - 40.0, "h": 20, "text": line, "size": 13, "color": "#374151"}),
        );
        b.node(
            id + 40 + j as NodeId,
            id,
            NodeKind::Rect,
            json!({"x": 20, "y": 144.0 + j as f32 * 26.0, "w": ww - 40.0, "h": 1, "color": "#eef1f5"}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Scene;

    #[test]
    fn demo_scene_builds_twenty_windows_plus_desktop_and_bar() {
        let mut scene = Scene::new();
        scene.apply_all(build(1440.0, 900.0, 20));
        assert_eq!(scene.top_level().len(), 22);
        let list = scene.draw_list();
        assert_eq!(list.layers.len(), 22);
        assert!(list.layers.iter().map(|l| l.texts.len()).sum::<usize>() > 100);
    }
}
