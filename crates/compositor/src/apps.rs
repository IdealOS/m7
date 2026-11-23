//! App windows: what `open_window` and `commit` do to the scene.
//!
//! Each app window gets a frame the compositor draws (background, title bar, title,
//! close button) and a content area, the app's node 1. An app names nodes with its
//! own ids, per window; they're translated to scene ids here, so an app can only
//! reach its own nodes and its content area, never another window or the desktop.
//! A commit is checked whole before any of it is applied, and only the connection
//! that opened a window may commit to it.
//!
//! Pointer presses on nodes an app listens to (and on the close button) become
//! [`WindowEvent`]s for that app; everything else stays with the window manager.
//! No I/O here: the bus side feeds requests in and sends the replies out.

use std::collections::HashMap;

use protocol::scene::{CONTENT, MAX_WINDOW_NODES};
use protocol::{Commit, NodeId, NodeKind, OpenWindowParams, Props, SceneOp, Sender, WindowEvent};
use serde_json::{Value, json};

use crate::scene::{ROOT, Scene};
use crate::wm::TITLE_BAR_HEIGHT;

/// Scene ids for app windows start here, well above the demo scene's.
const FIRST_NODE: NodeId = 1 << 32;
const DEFAULT_SIZE: (u32, u32) = (360, 240);
/// Space around and between placed windows.
const MARGIN: f32 = 60.0;
const GAP: f32 = 24.0;

/// A window event for the app whose request opened the window.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    /// The bus id of the app's `open_window` request.
    pub request: u64,
    pub event: WindowEvent,
}

struct Window {
    owner: Sender,
    request: u64,
    /// The frame (a root child) and the content area, as scene ids.
    frame: NodeId,
    content: NodeId,
    /// The app's node ids to scene ids.
    ids: HashMap<NodeId, NodeId>,
}

/// What a listened-to scene node belongs to.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Target {
    /// An app node: the window and the app's id for it.
    Node(u64, NodeId),
    /// A window's close button.
    Close(u64),
}

#[derive(Default)]
pub struct Apps {
    windows: HashMap<u64, Window>,
    by_request: HashMap<u64, u64>,
    /// Scene nodes that turn presses into events.
    targets: HashMap<NodeId, Target>,
    next_window: u64,
    next_node: NodeId,
    /// The node the pointer went down on, until it comes up.
    pressed: Option<NodeId>,
    /// Where the next window goes, and the bottom of the current row of windows.
    next_spot: (f32, f32),
    row_bottom: f32,
}

impl Apps {
    pub fn new() -> Apps {
        Apps { next_window: 1, next_node: FIRST_NODE, next_spot: (MARGIN, MARGIN), ..Apps::default() }
    }

    /// Where to put a new `w` by `h` window: beside the last one, then on a new row
    /// below, and once the screen is full, cascading from the top left again. A real
    /// placement policy comes with the window manager in M5.
    fn place(&mut self, w: f32, h: f32, screen: (f32, f32)) -> (f32, f32) {
        let (mut x, mut y) = self.next_spot;
        if x + w > screen.0 - MARGIN && x > MARGIN {
            (x, y) = (MARGIN, self.row_bottom + GAP);
        }
        if y + h > screen.1 - MARGIN && y > MARGIN {
            // Full: cascade, and start the rows over.
            let step = 32.0 * (self.next_window % 8) as f32;
            self.row_bottom = 0.0;
            self.next_spot = (MARGIN, MARGIN);
            return (
                (MARGIN + step).min((screen.0 - w).max(0.0)),
                (MARGIN + step).min((screen.1 - h).max(0.0)),
            );
        }
        self.next_spot = (x + w + GAP, y);
        self.row_bottom = self.row_bottom.max(y + h);
        (x, y)
    }

    pub fn len(&self) -> usize {
        self.windows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Open a window for `owner`'s streamed request `request`, on a `screen`-sized
    /// desktop. Returns the first part of the answer.
    pub fn open(
        &mut self,
        scene: &mut Scene,
        owner: Sender,
        request: u64,
        params: &OpenWindowParams,
        screen: (f32, f32),
    ) -> Reply {
        let window = self.next_window;
        self.next_window += 1;
        let w = params.width.unwrap_or(DEFAULT_SIZE.0).clamp(80, 4096);
        let h = params.height.unwrap_or(DEFAULT_SIZE.1).clamp(40, 4096);
        let (x, y) = self.place(w as f32, h as f32 + TITLE_BAR_HEIGHT, screen);
        let [frame, bg, bar, title, close, content] = std::array::from_fn(|_| self.alloc());
        let fw = w as f32;
        let ops = [
            create(
                frame,
                NodeKind::Group,
                json!({"window": true, "x": x, "y": y, "w": fw, "h": h as f32 + TITLE_BAR_HEIGHT, "clip": true}),
            ),
            create(
                bg,
                NodeKind::Rect,
                json!({"w": fw, "h": h as f32 + TITLE_BAR_HEIGHT, "color": "#ffffff", "radius": 10, "border_color": "#b8c1cc", "border_width": 1}),
            ),
            create(
                bar,
                NodeKind::Rect,
                json!({"w": fw, "h": TITLE_BAR_HEIGHT, "color": "#f3f5f8", "radius": 10}),
            ),
            create(
                title,
                NodeKind::Text,
                json!({"x": 14, "y": 10, "w": fw - 60.0, "h": 18, "text": params.title, "size": 13, "weight": "bold", "color": "#1f2937"}),
            ),
            create(
                close,
                NodeKind::Button,
                json!({"x": fw - 34.0, "y": 6, "w": 24, "h": 24, "label": "×", "size": 15}),
            ),
            create(
                content,
                NodeKind::Group,
                json!({"y": TITLE_BAR_HEIGHT, "w": fw, "h": h as f32, "clip": true}),
            ),
            insert(frame, bg),
            insert(frame, bar),
            insert(frame, title),
            insert(frame, close),
            insert(frame, content),
            insert(ROOT, frame),
        ];
        scene.apply_all(ops);
        self.targets.insert(close, Target::Close(window));
        self.windows.insert(window, Window { owner, request, frame, content, ids: HashMap::new() });
        self.by_request.insert(request, window);
        Reply { request, event: WindowEvent::Opened { window, width: w, height: h } }
    }

    /// Apply `commit` from `from`. Nothing is applied unless every op is valid.
    pub fn commit(&mut self, scene: &mut Scene, from: &Sender, commit: Commit) -> Result<(), String> {
        let Some(w) = self.windows.get(&commit.window) else {
            return Err(format!("no window {}", commit.window));
        };
        if &w.owner != from {
            return Err(format!("window {} belongs to another connection", commit.window));
        }
        let mut ops = Vec::with_capacity(commit.ops.len());
        let mut listens = Vec::new();
        let mut result = Ok(());
        for op in commit.ops {
            match self.translate(commit.window, op) {
                Ok(Translated::Op(op)) => ops.push(op),
                Ok(Translated::Listen(global, app_id, events)) => listens.push((global, app_id, events)),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if result.is_ok() {
            scene.apply_all(ops);
            for (global, app_id, events) in listens {
                if events.iter().any(|e| e == "click") {
                    self.targets.insert(global, Target::Node(commit.window, app_id));
                } else {
                    self.targets.remove(&global);
                }
            }
        }
        // Forget ids whose nodes are gone: destroyed, or allocated by a commit that
        // was refused.
        self.prune(scene, commit.window);
        result
    }

    fn translate(&mut self, window: u64, op: SceneOp) -> Result<Translated, String> {
        let next = &mut self.next_node;
        let w = self.windows.get_mut(&window).expect("caller checked");
        let lookup = |w: &Window, id: NodeId| -> Result<NodeId, String> {
            match id {
                CONTENT => Ok(w.content),
                _ => w.ids.get(&id).copied().ok_or_else(|| format!("no node {id} in this window")),
            }
        };
        // Only `insert` and `remove` may name the content area, as the parent.
        let own = |w: &Window, id: NodeId| -> Result<NodeId, String> {
            if id <= CONTENT {
                return Err(format!("node {id} is the window's; use ids above {CONTENT}"));
            }
            lookup(w, id)
        };
        Ok(Translated::Op(match op {
            SceneOp::Create { id, kind, props } => {
                if id <= CONTENT {
                    return Err(format!("node {id} is the window's; create ids above {CONTENT}"));
                }
                let global = match w.ids.get(&id) {
                    Some(&g) => g,
                    None => {
                        if w.ids.len() >= MAX_WINDOW_NODES {
                            return Err(format!("a window holds at most {MAX_WINDOW_NODES} nodes"));
                        }
                        let g = *next;
                        *next += 1;
                        w.ids.insert(id, g);
                        g
                    }
                };
                SceneOp::Create { id: global, kind, props: app_props(props) }
            }
            SceneOp::Set { id, props } => SceneOp::Set { id: own(w, id)?, props: app_props(props) },
            SceneOp::Insert { parent, child, index } => {
                SceneOp::Insert { parent: lookup(w, parent)?, child: own(w, child)?, index }
            }
            SceneOp::Remove { parent, child } => {
                SceneOp::Remove { parent: lookup(w, parent)?, child: own(w, child)? }
            }
            SceneOp::Destroy { id } => SceneOp::Destroy { id: own(w, id)? },
            SceneOp::Listen { id, events } => return Ok(Translated::Listen(own(w, id)?, id, events)),
        }))
    }

    fn prune(&mut self, scene: &Scene, window: u64) {
        let Some(w) = self.windows.get_mut(&window) else { return };
        w.ids.retain(|_, g| scene.node(*g).is_some());
        self.targets.retain(|g, _| scene.node(*g).is_some());
    }

    /// The app's request `request` ended (cancelled, answered, or its connection
    /// closed): remove its window. Returns whether there was one.
    pub fn close_request(&mut self, scene: &mut Scene, request: u64) -> bool {
        let Some(window) = self.by_request.remove(&request) else { return false };
        let w = self.windows.remove(&window).expect("indexes agree");
        scene.apply(SceneOp::Destroy { id: w.frame });
        self.targets.retain(|g, _| scene.node(*g).is_some());
        if self.pressed.is_some_and(|p| scene.node(p).is_none()) {
            self.pressed = None;
        }
        true
    }

    /// The pointer went down at (`x`, `y`) in root child `window` (the topmost
    /// window there). Returns true if it landed on a node that takes presses, which
    /// is then shown pressed until the pointer comes up.
    pub fn press(&mut self, scene: &mut Scene, window: NodeId, x: f32, y: f32) -> bool {
        let Some(node) = scene.hit(window, x, y, |id| self.targets.contains_key(&id)) else { return false };
        self.pressed = Some(node);
        scene.apply(SceneOp::Set { id: node, props: props(json!({"pressed": true})) });
        true
    }

    /// Whether a press is being held on one of the windows' nodes.
    pub fn pressing(&self) -> bool {
        self.pressed.is_some()
    }

    /// The pointer came up at (`x`, `y`). If it's still over the node it went down
    /// on, that's a click: returns the event for the app.
    pub fn release(&mut self, scene: &mut Scene, x: f32, y: f32) -> Option<Reply> {
        let node = self.pressed.take()?;
        scene.apply(SceneOp::Set { id: node, props: props(json!({"pressed": false})) });
        let target = *self.targets.get(&node)?;
        let window = match target {
            Target::Node(w, _) | Target::Close(w) => w,
        };
        let w = self.windows.get(&window)?;
        // Dragged off before letting go: no click.
        scene.hit(w.frame, x, y, |id| id == node)?;
        let event = match target {
            Target::Node(_, app_id) => WindowEvent::Event { node: app_id, event: "click".into() },
            Target::Close(_) => WindowEvent::Close,
        };
        Some(Reply { request: w.request, event })
    }

    fn alloc(&mut self) -> NodeId {
        let id = self.next_node;
        self.next_node += 1;
        id
    }
}

enum Translated {
    Op(SceneOp),
    /// A scene node, the app's id for it, and the events it listens for.
    Listen(NodeId, NodeId, Vec<String>),
}

/// An app's props, minus the ones only the compositor may set.
fn app_props(mut p: Props) -> Props {
    p.remove("window");
    p.remove("pressed");
    p
}

fn props(v: Value) -> Props {
    v.as_object().expect("props are objects").clone()
}

fn create(id: NodeId, kind: NodeKind, p: Value) -> SceneOp {
    SceneOp::Create { id, kind, props: props(p) }
}

fn insert(parent: NodeId, child: NodeId) -> SceneOp {
    SceneOp::Insert { parent, child, index: usize::MAX }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(conn: u64) -> Sender {
        Sender::App { app: "org.example.counter".into(), conn }
    }

    fn open(apps: &mut Apps, scene: &mut Scene, owner: Sender, request: u64) -> u64 {
        let params = OpenWindowParams { title: "Counter".into(), width: Some(300), height: Some(200) };
        match apps.open(scene, owner, request, &params, (1280.0, 800.0)).event {
            WindowEvent::Opened { window, width, height } => {
                assert_eq!((width, height), (300, 200));
                window
            }
            other => panic!("{other:?}"),
        }
    }

    fn ops(v: Value) -> Vec<SceneOp> {
        serde_json::from_value(v).unwrap()
    }

    /// The counter app's first commit: a column with a label and a button.
    fn counter_ui() -> Vec<SceneOp> {
        ops(json!([
            {"op": "create", "id": 2, "type": "vstack", "props": {"padding": 16, "gap": 12}},
            {"op": "create", "id": 3, "type": "label", "props": {"text": "Count: 0", "size": 20}},
            {"op": "create", "id": 4, "type": "button", "props": {"label": "Add one"}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0},
            {"op": "insert", "parent": 2, "child": 3, "index": 0},
            {"op": "insert", "parent": 2, "child": 4, "index": 1},
            {"op": "listen", "id": 4, "events": ["click"]}
        ]))
    }

    fn text_of(scene: &Scene, apps: &Apps, window: u64, id: NodeId) -> String {
        let g = apps.windows[&window].ids[&id];
        scene.node(g).unwrap().props["text"].as_str().unwrap().to_owned()
    }

    /// Where a node is drawn, from the draw list's text items.
    fn find_button(scene: &Scene, apps: &Apps, window: u64, id: NodeId) -> (f32, f32) {
        let g = apps.windows[&window].ids[&id];
        let list = scene.draw_list();
        let t = list.layers.iter().flat_map(|l| &l.texts).find(|t| t.node == g).unwrap();
        (t.rect.x + t.rect.w / 2.0, t.rect.y + t.rect.h / 2.0)
    }

    #[test]
    fn an_app_builds_its_window_and_hears_clicks() {
        let mut scene = Scene::new();
        let mut apps = Apps::new();
        let w = open(&mut apps, &mut scene, app(5), 77);
        apps.commit(&mut scene, &app(5), Commit { window: w, ops: counter_ui() }).unwrap();
        assert_eq!(text_of(&scene, &apps, w, 3), "Count: 0");

        // The stack laid the button out below the label, inside the content area.
        let (bx, by) = find_button(&scene, &apps, w, 4);
        let frame = apps.windows[&w].frame;
        assert!(apps.press(&mut scene, frame, bx, by));
        let pressed = apps.windows[&w].ids[&4];
        assert_eq!(scene.node(pressed).unwrap().props["pressed"], true);
        let reply = apps.release(&mut scene, bx, by).unwrap();
        assert_eq!(
            reply,
            Reply { request: 77, event: WindowEvent::Event { node: 4, event: "click".into() } }
        );
        assert_eq!(scene.node(pressed).unwrap().props["pressed"], false);

        // The app answers by changing the label.
        let set = ops(json!([{"op": "set", "id": 3, "props": {"text": "Count: 1"}}]));
        apps.commit(&mut scene, &app(5), Commit { window: w, ops: set }).unwrap();
        assert_eq!(text_of(&scene, &apps, w, 3), "Count: 1");

        // A press dragged off the button before release is no click.
        assert!(apps.press(&mut scene, frame, bx, by));
        assert_eq!(apps.release(&mut scene, 1.0, 1.0), None);
        // Pressing the label (which doesn't listen) is no press at all.
        let label = apps.windows[&w].ids[&3];
        let list = scene.draw_list();
        let t = list.layers.iter().flat_map(|l| &l.texts).find(|t| t.node == label).unwrap();
        assert!(!apps.press(&mut scene, frame, t.rect.x + 2.0, t.rect.y + 2.0));
    }

    #[test]
    fn the_close_button_asks_the_app_and_ending_the_request_removes_the_window() {
        let mut scene = Scene::new();
        let mut apps = Apps::new();
        let w = open(&mut apps, &mut scene, app(5), 9);
        apps.commit(&mut scene, &app(5), Commit { window: w, ops: counter_ui() }).unwrap();
        let frame = apps.windows[&w].frame;
        let r = scene.local_rect(frame).unwrap();
        let (cx, cy) = (r.x + r.w - 22.0, r.y + 18.0);
        assert!(apps.press(&mut scene, frame, cx, cy));
        assert_eq!(apps.release(&mut scene, cx, cy), Some(Reply { request: 9, event: WindowEvent::Close }));
        assert!(scene.node(frame).is_some(), "the app decides");

        let before = scene.len();
        assert!(apps.close_request(&mut scene, 9));
        assert!(scene.node(frame).is_none());
        assert_eq!(before - scene.len(), 6 + 3, "the frame's six nodes and the app's three");
        assert!(apps.is_empty() && apps.targets.is_empty());
        assert!(!apps.close_request(&mut scene, 9));
    }

    #[test]
    fn apps_reach_only_their_own_nodes() {
        let mut scene = Scene::new();
        let mut apps = Apps::new();
        let mine = open(&mut apps, &mut scene, app(5), 1);
        let theirs = open(&mut apps, &mut scene, app(6), 2);
        apps.commit(&mut scene, &app(6), Commit { window: theirs, ops: counter_ui() }).unwrap();

        let refused = |apps: &mut Apps, scene: &mut Scene, from: Sender, window: u64, v: Value| {
            let n = scene.len();
            let r = apps.commit(scene, &from, Commit { window, ops: ops(v) });
            assert!(r.is_err(), "{r:?}");
            assert_eq!(scene.len(), n, "nothing applied");
        };
        // Another connection can't commit to the window, even with the same app id.
        refused(
            &mut apps,
            &mut scene,
            app(5),
            theirs,
            json!([{"op": "set", "id": 3, "props": {"text": "pwned"}}]),
        );
        // Ids are per window: node 3 isn't in this window, and 0 and 1 aren't the app's.
        refused(&mut apps, &mut scene, app(5), mine, json!([{"op": "set", "id": 3, "props": {}}]));
        refused(&mut apps, &mut scene, app(5), mine, json!([{"op": "destroy", "id": 1}]));
        refused(&mut apps, &mut scene, app(5), mine, json!([{"op": "create", "id": 0, "type": "rect"}]));
        refused(&mut apps, &mut scene, app(5), mine, json!([{"op": "create", "id": 1, "type": "rect"}]));
        refused(&mut apps, &mut scene, app(5), 99, json!([]));
        // A bad op anywhere in a commit refuses all of it, including creates before it.
        refused(
            &mut apps,
            &mut scene,
            app(5),
            mine,
            json!([{"op": "create", "id": 2, "type": "rect"}, {"op": "insert", "parent": 0, "child": 2, "index": 0}]),
        );
        assert!(apps.windows[&mine].ids.is_empty(), "refused creates leave no ids behind");
        assert_eq!(text_of(&scene, &apps, theirs, 3), "Count: 0");

        // An app can't make its nodes look like windows or pressed buttons.
        let sneaky = ops(json!([
            {"op": "create", "id": 2, "type": "group", "props": {"window": true, "pressed": true}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0}
        ]));
        apps.commit(&mut scene, &app(5), Commit { window: mine, ops: sneaky }).unwrap();
        let g = apps.windows[&mine].ids[&2];
        assert!(scene.node(g).unwrap().props.get("window").is_none());
        assert!(scene.node(g).unwrap().props.get("pressed").is_none());
    }

    #[test]
    fn destroying_nodes_forgets_their_ids_and_listeners() {
        let mut scene = Scene::new();
        let mut apps = Apps::new();
        let w = open(&mut apps, &mut scene, app(5), 1);
        apps.commit(&mut scene, &app(5), Commit { window: w, ops: counter_ui() }).unwrap();
        assert_eq!(apps.windows[&w].ids.len(), 3);
        apps.commit(&mut scene, &app(5), Commit { window: w, ops: ops(json!([{"op": "destroy", "id": 2}])) })
            .unwrap();
        assert!(apps.windows[&w].ids.is_empty(), "the stack and its children");
        assert_eq!(apps.targets.len(), 1, "only the close button");
        // The id can be used again.
        apps.commit(&mut scene, &app(5), Commit { window: w, ops: counter_ui() }).unwrap();
        assert_eq!(text_of(&scene, &apps, w, 3), "Count: 0");
    }

    #[test]
    fn new_windows_go_side_by_side_then_in_rows_then_cascade() {
        let mut scene = Scene::new();
        let mut apps = Apps::new();
        let spot = |apps: &mut Apps, scene: &mut Scene, n: u64| {
            let p = OpenWindowParams { title: "w".into(), width: Some(300), height: Some(200) };
            apps.open(scene, app(n), n, &p, (1000.0, 700.0));
            let w = apps.windows.values().max_by_key(|w| w.frame).unwrap();
            let r = scene.local_rect(w.frame).unwrap();
            (r.x, r.y)
        };
        assert_eq!(spot(&mut apps, &mut scene, 1), (60.0, 60.0));
        assert_eq!(spot(&mut apps, &mut scene, 2), (384.0, 60.0), "beside the first");
        assert_eq!(spot(&mut apps, &mut scene, 3), (60.0, 320.0), "a new row: 60 + 236 tall + 24");
        assert_eq!(spot(&mut apps, &mut scene, 4), (384.0, 320.0));
        let (x, y) = spot(&mut apps, &mut scene, 5);
        assert!(
            x >= 0.0 && y >= 0.0 && x + 300.0 <= 1000.0 && y + 236.0 <= 700.0,
            "full: cascaded on screen ({x}, {y})"
        );
    }

    #[test]
    fn windows_hold_a_bounded_number_of_nodes() {
        let mut scene = Scene::new();
        let mut apps = Apps::new();
        let w = open(&mut apps, &mut scene, app(5), 1);
        let many: Vec<SceneOp> = (2..MAX_WINDOW_NODES as u64 + 3)
            .map(|id| SceneOp::Create { id, kind: NodeKind::Rect, props: Props::new() })
            .collect();
        assert!(apps.commit(&mut scene, &app(5), Commit { window: w, ops: many }).is_err());
        assert!(apps.windows[&w].ids.is_empty());
    }
}
