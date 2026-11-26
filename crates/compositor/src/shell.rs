//! The shell: the top bar's launcher, workspace indicator and clock.
//!
//! It lives in the compositor (it needs to launch apps, which only system services
//! may ask session to do, and it must stay above every window). It draws into the
//! demo scene's top bar: an "Apps" button that opens a menu of installed apps,
//! one button per workspace with the current one highlighted, the frame-time
//! readout, and a clock.

use protocol::{NodeId, NodeKind, SceneOp};
use serde_json::{Value, json};

use crate::demo::{HUD, TOP_BAR};
use crate::scene::{ROOT, Rect, Scene};

/// How many workspaces there are.
pub const WORKSPACES: u32 = 4;
/// The top bar's height.
pub const BAR_HEIGHT: f32 = 30.0;

const TITLE: NodeId = 12;
pub const LAUNCHER: NodeId = 13;
const FIRST_WORKSPACE: NodeId = 14;
const CLOCK: NodeId = 20;

/// What a press on the bar asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Launcher,
    Workspace(u32),
}

fn props(v: Value) -> protocol::Props {
    v.as_object().expect("props are objects").clone()
}

fn add(scene: &mut Scene, id: NodeId, kind: NodeKind, p: Value) {
    if scene.node(id).is_none() {
        scene.apply(SceneOp::Create { id, kind, props: props(p) });
        scene.apply(SceneOp::Insert { parent: TOP_BAR, child: id, index: usize::MAX });
    } else {
        scene.apply(SceneOp::Set { id, props: props(p) });
    }
}

/// Put the shell into the top bar (once the demo scene has made it), for a
/// screen `w` wide.
pub fn install(scene: &mut Scene, w: f32, workspace: u32) {
    if scene.node(TOP_BAR).is_none() {
        // No demo scene: a bar of its own.
        scene.apply(SceneOp::Create {
            id: TOP_BAR,
            kind: NodeKind::Group,
            props: props(json!({"w": w, "h": BAR_HEIGHT})),
        });
        scene.apply(SceneOp::Insert { parent: ROOT, child: TOP_BAR, index: usize::MAX });
        add(scene, 10, NodeKind::Rect, json!({"w": w, "h": BAR_HEIGHT, "color": "$bar"}));
    }
    if scene.node(TITLE).is_some() {
        scene.apply(SceneOp::Set { id: TITLE, props: props(json!({"hidden": true})) });
    }
    add(
        scene,
        LAUNCHER,
        NodeKind::Button,
        json!({"x": 8, "y": 3, "w": 64, "h": 24, "label": "Apps", "size": 13}),
    );
    for n in 0..WORKSPACES {
        add(
            scene,
            FIRST_WORKSPACE + n as NodeId,
            NodeKind::Button,
            json!({"x": 84.0 + n as f32 * 30.0, "y": 3, "w": 26, "h": 24, "label": (n + 1).to_string(), "size": 12}),
        );
    }
    add(
        scene,
        CLOCK,
        NodeKind::Text,
        json!({"x": w - 62.0, "y": 7, "w": 54, "h": 18, "text": "", "size": 13, "weight": "bold", "color": "$text", "align": "center"}),
    );
    if scene.node(HUD).is_some() {
        scene.apply(SceneOp::Set { id: HUD, props: props(json!({"x": w - 390.0})) });
    }
    set_workspace(scene, workspace);
}

/// Highlight workspace `n`'s button.
pub fn set_workspace(scene: &mut Scene, n: u32) {
    for i in 0..WORKSPACES {
        let current = i + 1 == n;
        let p = if current {
            json!({"color": "$accent", "text_color": "$accent-text"})
        } else {
            json!({"color": "$control", "text_color": "$text"})
        };
        scene.apply(SceneOp::Set { id: FIRST_WORKSPACE + i as NodeId, props: props(p) });
    }
}

/// Show `text` (such as `14:05`) as the time.
pub fn set_clock(scene: &mut Scene, text: &str) {
    if scene.node(CLOCK).and_then(|n| n.props.get("text")).and_then(Value::as_str) != Some(text) {
        scene.apply(SceneOp::Set { id: CLOCK, props: props(json!({"text": text})) });
    }
}

/// What a press at (`x`, `y`) on the bar asks for, if anything.
pub fn press(scene: &Scene, x: f32, y: f32) -> Option<Action> {
    if y >= BAR_HEIGHT {
        return None;
    }
    let hit = |id: NodeId| scene.local_rect(id).is_some_and(|r: Rect| r.contains(x, y));
    if hit(LAUNCHER) {
        return Some(Action::Launcher);
    }
    (0..WORKSPACES).find(|&n| hit(FIRST_WORKSPACE + n as NodeId)).map(|n| Action::Workspace(n + 1))
}

/// Where the launcher button is, to open its menu under it.
pub fn launcher_rect(scene: &Scene) -> Rect {
    scene.local_rect(LAUNCHER).unwrap_or(Rect { x: 8.0, y: 3.0, w: 64.0, h: 24.0 })
}

/// The time of day as `HH:MM`, in the device's local time.
pub fn clock_text(now: std::time::SystemTime) -> String {
    let secs = now.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let local = secs + local_offset(secs);
    let minutes = local.div_euclid(60);
    format!("{:02}:{:02}", minutes.div_euclid(60).rem_euclid(24), minutes.rem_euclid(60))
}

/// The local time zone's offset from UTC at `secs`, in seconds.
fn local_offset(secs: i64) -> i64 {
    // SAFETY: localtime_r writes only into `tm`, which outlives the call.
    unsafe {
        let t: libc::time_t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() { 0 } else { tm.tm_gmtoff as i64 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;

    #[test]
    fn the_bar_has_a_launcher_workspaces_and_a_clock() {
        let mut scene = Scene::new();
        scene.apply_all(demo::build(1280.0, 800.0, 2));
        install(&mut scene, 1280.0, 1);
        assert_eq!(press(&scene, 20.0, 15.0), Some(Action::Launcher));
        assert_eq!(press(&scene, 84.0 + 30.0 + 5.0, 15.0), Some(Action::Workspace(2)));
        assert_eq!(press(&scene, 600.0, 15.0), None);
        assert_eq!(press(&scene, 20.0, 40.0), None, "below the bar");
        set_workspace(&mut scene, 3);
        assert_eq!(scene.node(FIRST_WORKSPACE + 2).unwrap().props["color"], "$accent");
        assert_eq!(scene.node(FIRST_WORKSPACE).unwrap().props["color"], "$control");
        set_clock(&mut scene, "09:41");
        assert_eq!(scene.node(CLOCK).unwrap().props["text"], "09:41");
        let t = clock_text(std::time::SystemTime::now());
        assert_eq!(t.len(), 5);
        assert_eq!(&t[2..3], ":");
    }
}
