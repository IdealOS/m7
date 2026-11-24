//! Permission prompts: the compositor's trusted layer.
//!
//! permd asks with `prompt`; the compositor shows one prompt at a time, in order,
//! over everything: every window, menu and the top bar, all dimmed behind it. No
//! app window can reach the top bar or dim the whole screen, so that is how a
//! real prompt looks different from an app drawing one in its own window. While
//! a prompt is up, it takes all input.
//!
//! The app's own words (its name and its reason) are shown as quotes beside its
//! id, never as the prompt's own text. "Don't allow" has the focus, so a stray
//! Enter or Space, typed into a window just as the prompt appeared, refuses.

use protocol::scene::PromptParams;
use protocol::{NodeId, NodeKind, SceneOp};
use serde_json::{Value, json};

use crate::scene::{ROOT, Rect};
use crate::theme;

/// The prompt's scene ids, above every other overlay's.
pub const PROMPT: NodeId = (1 << 40) + 20_000;

const CARD_W: f32 = 460.0;
const PAD: f32 = 20.0;
const LINE: f32 = 22.0;
const BUTTON_W: f32 = 120.0;
const BUTTON_H: f32 = 34.0;

/// Which button a press or key chose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Choice {
    Allow,
    Deny,
}

/// Where the shown prompt's buttons are, for presses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shown {
    pub allow: Rect,
    pub deny: Rect,
}

impl Shown {
    pub fn hit(&self, x: f32, y: f32) -> Option<Choice> {
        let inside = |r: &Rect| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
        if inside(&self.allow) {
            Some(Choice::Allow)
        } else if inside(&self.deny) {
            Some(Choice::Deny)
        } else {
            None
        }
    }
}

fn create(id: NodeId, kind: NodeKind, props: Value) -> SceneOp {
    SceneOp::Create { id, kind, props: props.as_object().cloned().unwrap_or_default() }
}

fn insert(parent: NodeId, child: NodeId) -> SceneOp {
    SceneOp::Insert { parent, child, index: usize::MAX }
}

/// At most `max` characters of `s`, with an ellipsis if cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// The ops that show prompt `p` on a `w` by `h` screen, with `allow_focused`
/// saying which button has the focus, and where its buttons are.
pub fn ops(p: &PromptParams, w: f32, h: f32, allow_focused: bool) -> (Vec<SceneOp>, Shown) {
    let asks: Vec<String> = p.asks.iter().take(8).map(|a| format!("•  {}", clip(a, 80))).collect();
    let reason = p.reason.as_deref().map(|r| format!("It says: “{}”", clip(r, 140)));
    let reason_lines = if reason.is_some() { 2.0 } else { 0.0 };
    let card_h = PAD
        + 26.0
        + 20.0
        + 12.0
        + LINE * asks.len() as f32
        + 8.0
        + LINE * reason_lines
        + 16.0
        + BUTTON_H
        + PAD;
    let (cx, cy) = (((w - CARD_W) / 2.0).max(0.0).round(), ((h - card_h) / 2.0).max(0.0).round());
    let mut ops = vec![
        create(PROMPT, NodeKind::Group, json!({"x": 0, "y": 0, "w": w, "h": h})),
        create(PROMPT + 1, NodeKind::Rect, json!({"w": w, "h": h, "color": theme::color::SCRIM})),
        create(
            PROMPT + 2,
            NodeKind::Rect,
            json!({"x": cx, "y": cy, "w": CARD_W, "h": card_h, "color": theme::color::SURFACE, "radius": theme::radius::M, "border_color": theme::color::ACCENT, "border_width": 2}),
        ),
        create(
            PROMPT + 3,
            NodeKind::Rect,
            json!({"x": cx, "y": cy, "w": CARD_W, "h": 6, "color": theme::color::ACCENT, "radius": theme::radius::M}),
        ),
        insert(ROOT, PROMPT),
    ];
    for id in PROMPT + 1..=PROMPT + 3 {
        ops.push(insert(PROMPT, id));
    }
    let mut y = cy + PAD;
    let mut next = PROMPT + 10;
    let mut text = |ops: &mut Vec<SceneOp>, y: f32, s: &str, size: f32, color: &str| {
        ops.push(create(next, NodeKind::Text, json!({"x": cx + PAD, "y": y, "w": CARD_W - 2.0 * PAD, "h": LINE, "text": s, "size": size, "color": color})));
        ops.push(insert(PROMPT, next));
        next += 1;
    };
    text(&mut ops, y, &format!("“{}” would like to", clip(&p.name, 40)), 18.0, theme::color::TEXT);
    y += 26.0;
    text(&mut ops, y, &format!("App id {}", clip(&p.app, 60)), 13.0, theme::color::MUTED);
    y += 20.0 + 12.0;
    for a in &asks {
        text(&mut ops, y, a, 15.0, theme::color::TEXT);
        y += LINE;
    }
    y += 8.0;
    if let Some(r) = &reason {
        text(&mut ops, y, r, 13.0, theme::color::MUTED);
        y += LINE * reason_lines;
    }
    y += 16.0;
    let allow = Rect { x: cx + CARD_W - PAD - BUTTON_W, y, w: BUTTON_W, h: BUTTON_H };
    let deny = Rect { x: allow.x - 12.0 - BUTTON_W, y, w: BUTTON_W, h: BUTTON_H };
    for (r, label, primary, focused) in
        [(deny, "Don't allow", false, !allow_focused), (allow, "Allow", true, allow_focused)]
    {
        let (fill, ink) = if primary {
            (theme::color::ACCENT, theme::color::ACCENT_TEXT)
        } else {
            (theme::color::CONTROL, theme::color::TEXT)
        };
        let border = if focused { theme::color::FOCUS } else { theme::color::CONTROL_BORDER };
        ops.push(create(next, NodeKind::Rect, json!({"x": r.x, "y": r.y, "w": r.w, "h": r.h, "color": fill, "radius": theme::radius::S, "border_color": border, "border_width": if focused { 3 } else { 1 }})));
        ops.push(insert(PROMPT, next));
        next += 1;
        ops.push(create(next, NodeKind::Text, json!({"x": r.x, "y": r.y + 8.0, "w": r.w, "h": 18, "text": label, "size": 15, "color": ink, "align": "center"})));
        ops.push(insert(PROMPT, next));
        next += 1;
    }
    (ops, Shown { allow, deny })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(reason: Option<&str>) -> PromptParams {
        PromptParams {
            app: "org.x.photos".into(),
            name: "Photos".into(),
            asks: vec!["read your Image documents".into()],
            reason: reason.map(str::to_owned),
        }
    }

    #[test]
    fn buttons_sit_inside_the_card_and_presses_find_them() {
        let (ops, shown) = ops(&params(Some("to show them")), 1280.0, 800.0, false);
        assert!(matches!(&ops[0], SceneOp::Create { id: PROMPT, .. }));
        let mid = |r: Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let (ax, ay) = mid(shown.allow);
        let (dx, dy) = mid(shown.deny);
        assert_eq!(shown.hit(ax, ay), Some(Choice::Allow));
        assert_eq!(shown.hit(dx, dy), Some(Choice::Deny));
        assert_eq!(shown.hit(5.0, 5.0), None, "the dimmed rest takes presses but chooses nothing");
        assert!(shown.deny.x < shown.allow.x);
    }

    #[test]
    fn the_apps_words_are_quoted_and_cut_short() {
        let long = "x".repeat(500);
        let (ops, _) = ops(&params(Some(&long)), 1280.0, 800.0, false);
        let texts: Vec<String> = ops
            .iter()
            .filter_map(|o| match o {
                SceneOp::Create { props, .. } => props.get("text").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect();
        assert_eq!(texts[0], "“Photos” would like to");
        assert_eq!(texts[1], "App id org.x.photos");
        assert!(texts.iter().any(|t| t.starts_with("It says: “") && t.chars().count() < 160));
    }
}
