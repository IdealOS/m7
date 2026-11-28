//! App windows: what `open_window` and `commit` do to the scene.
//!
//! Each app window gets a frame the compositor draws (background, title bar, title,
//! close button) and a content area, the app's node 1. An app names nodes with its
//! own ids, per window; they're translated to scene ids here, so an app can only
//! reach its own nodes and its content area, never another window or the desktop.
//! A commit is checked whole before any of it is applied, and only the connection
//! that opened a window may commit to it.
//!
//! Pointer presses on nodes an app listens to (and on the close and maximize
//! buttons) become [`WindowEvent`]s for that app; widgets the desktop drives send
//! theirs through [`Apps::event`]. Windows can be resized and maximized, which
//! tells the app its new content size. No I/O here: the bus side feeds requests in
//! and sends the replies out.
//!
//! Text widgets whose app listens for `select` report where their caret and
//! selection are. An app moves them by setting `selection: [anchor, caret]`,
//! focuses a widget by setting `focus: true`, and adds change sets it made to a
//! bound field's undo history with `undo_group: [change sets]`; those are
//! requests, not props kept on the node, handed to the desktop through
//! [`Apps::take_asked`].

use std::collections::{HashMap, HashSet};

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

/// Something an app asked of one of its widgets in a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asked {
    /// Select from `anchor` to `caret` (character indexes) in a text widget.
    Select(usize, usize),
    /// Take the keyboard focus.
    Focus,
    /// Change sets the app made to a bound field's document, to undo with the
    /// field's own edits (Command-Z), as one step.
    Undoable(Vec<i64>),
}

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
    /// The frame's own parts, which resizing moves: background, title bar, its
    /// filler (square bottom corners), the line under it, title, close, maximize.
    parts: [NodeId; 7],
    /// The content area's size.
    size: (u32, u32),
    /// Where and how big it was before it was maximized.
    restore: Option<(f32, f32, u32, u32)>,
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
    /// A window's maximize button.
    Max(u64),
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
    /// Each app node's window and app id, by scene id, for widget events.
    owners: HashMap<NodeId, (u64, NodeId)>,
    /// The area maximized windows fill: below the top bar.
    work: (f32, f32, f32, f32),
    /// The workspace new windows open on.
    pub workspace: u32,
    /// Text each text widget reported to its app (`change` events) that the app
    /// hasn't sent back yet, oldest first. See [`Apps::reported`].
    reported: HashMap<NodeId, std::collections::VecDeque<String>>,
    /// Text widgets whose app listens for `select`.
    selects: HashSet<NodeId>,
    /// What apps asked of their widgets in commits, not yet done.
    asked: Vec<(NodeId, Asked)>,
}

/// How many unanswered reports a text widget remembers.
const MAX_REPORTED: usize = 64;

impl Apps {
    pub fn new() -> Apps {
        Apps {
            next_window: 1,
            next_node: FIRST_NODE,
            next_spot: (MARGIN, MARGIN),
            work: (0.0, 30.0, 1280.0, 770.0),
            workspace: 1,
            ..Apps::default()
        }
    }

    /// The area maximized windows fill.
    pub fn set_work_area(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.work = (x, y, w, h);
    }

    /// The app window whose frame is scene node `frame`.
    pub fn window_of(&self, frame: NodeId) -> Option<u64> {
        self.windows.iter().find(|(_, w)| w.frame == frame).map(|(&id, _)| id)
    }

    /// Window `window`'s frame (a root child).
    pub fn frame_of(&self, window: u64) -> Option<NodeId> {
        self.windows.get(&window).map(|w| w.frame)
    }

    /// The frame of a window of app `app` (its newest, if it has several).
    pub fn frame_of_app(&self, app: &str) -> Option<NodeId> {
        self.windows
            .iter()
            .filter(|(_, w)| matches!(&w.owner, Sender::App { app: a, .. } if a == app))
            .max_by_key(|(id, _)| **id)
            .map(|(_, w)| w.frame)
    }

    /// The app that owns scene node `node` (by its window), if an app does: who the
    /// compositor acts for when it reads or writes documents for that node.
    pub fn app_of(&self, node: NodeId) -> Option<&str> {
        let &(window, _) = self.owners.get(&node)?;
        match &self.windows.get(&window)?.owner {
            Sender::App { app, .. } => Some(app),
            _ => None,
        }
    }

    /// An event for the app that owns scene node `node`, if an app does.
    pub fn event(&self, node: NodeId, event: &str, value: Option<Value>) -> Option<Reply> {
        let &(window, app_id) = self.owners.get(&node)?;
        let w = self.windows.get(&window)?;
        Some(Reply {
            request: w.request,
            event: WindowEvent::Event { node: app_id, event: event.into(), value },
        })
    }

    /// Text widget `node` told its app its text is now `text`.
    ///
    /// Apps often set a field's `value` from its `change` events. Those echoes
    /// arrive late when someone types quickly; applied, each would put the field
    /// back to an older text, losing what was typed since. So an app setting a
    /// value it was told is taken as an echo and leaves the shown text alone; any
    /// other value replaces it.
    pub fn reported(&mut self, node: NodeId, text: &str) {
        let q = self.reported.entry(node).or_default();
        if q.len() == MAX_REPORTED {
            q.pop_front();
        }
        q.push_back(text.to_owned());
    }

    /// Whether the app setting text widget `node` to `value` is an echo of
    /// something it was told (forgetting that report and older ones), rather
    /// than new text.
    fn is_echo(&mut self, node: NodeId, value: &Value) -> bool {
        let Some(q) = self.reported.get_mut(&node) else { return false };
        match value.as_str().and_then(|v| q.iter().position(|r| r == v)) {
            Some(i) => {
                q.drain(..=i);
                true
            }
            None => {
                q.clear();
                false
            }
        }
    }

    /// Resize window `window`'s content area to `w` by `h` (kept to sensible
    /// limits). Returns the event telling the app, if the size changed.
    pub fn resize(&mut self, scene: &mut Scene, window: u64, w: u32, h: u32) -> Option<Reply> {
        let win = self.windows.get_mut(&window)?;
        let (w, h) = (w.clamp(120, 4096), h.clamp(60, 4096));
        if (w, h) == win.size {
            return None;
        }
        win.size = (w, h);
        let (fw, fh) = (w as f32, h as f32 + TITLE_BAR_HEIGHT);
        let [bg, bar, fill, line, title, close, max] = win.parts;
        scene.apply_all([
            set(win.frame, json!({"w": fw, "h": fh})),
            set(bg, json!({"w": fw, "h": fh})),
            set(bar, json!({"w": fw - 2.0})),
            set(fill, json!({"w": fw - 2.0})),
            set(line, json!({"w": fw - 2.0})),
            set(title, json!({"w": (fw - 90.0).max(0.0)})),
            set(close, json!({"x": fw - 34.0})),
            set(max, json!({"x": fw - 62.0})),
            set(win.content, json!({"w": fw, "h": h as f32})),
        ]);
        Some(Reply { request: win.request, event: WindowEvent::Resized { width: w, height: h } })
    }

    /// Maximize window `window` to the work area, or put it back.
    pub fn toggle_maximize(&mut self, scene: &mut Scene, window: u64) -> Option<Reply> {
        let win = self.windows.get_mut(&window)?;
        let frame = win.frame;
        let (x, y, w, h) = match win.restore.take() {
            Some(r) => r,
            None => {
                let at = scene.local_rect(frame)?;
                win.restore = Some((at.x, at.y, win.size.0, win.size.1));
                let (wx, wy, ww, wh) = self.work;
                (wx, wy, ww as u32, (wh - TITLE_BAR_HEIGHT).max(60.0) as u32)
            }
        };
        scene.apply(set(frame, json!({"x": x, "y": y})));
        self.resize(scene, window, w, h)
    }

    /// Whether window `window` is maximized.
    pub fn is_maximized(&self, window: u64) -> bool {
        self.windows.get(&window).is_some_and(|w| w.restore.is_some())
    }

    /// Window `window`'s title bar parts, which take its hover, pressed and active
    /// states: the bar, its filler and the title.
    pub fn title_parts(&self, window: u64) -> Option<[NodeId; 3]> {
        self.windows.get(&window).map(|w| [w.parts[1], w.parts[2], w.parts[4]])
    }

    /// Window `window`'s content area, as a scene id.
    pub fn content_of(&self, window: u64) -> Option<NodeId> {
        self.windows.get(&window).map(|w| w.content)
    }

    /// The content size of window `window`.
    pub fn size_of(&self, window: u64) -> Option<(u32, u32)> {
        self.windows.get(&window).map(|w| w.size)
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
        let [frame, bg, bar, fill, line, title, close, max, content] = std::array::from_fn(|_| self.alloc());
        let fw = w as f32;
        let ops = [
            create(
                frame,
                NodeKind::Group,
                json!({"window": true, "x": x, "y": y, "w": fw, "h": h as f32 + TITLE_BAR_HEIGHT, "clip": true,
                       "title": params.title, "workspace": self.workspace}),
            ),
            create(
                bg,
                NodeKind::Rect,
                json!({"w": fw, "h": h as f32 + TITLE_BAR_HEIGHT, "color": "$surface", "radius": 10, "border_color": "$border", "border_width": 1}),
            ),
            // The title bar, styled by the system sheet (`.titlebar`, with :hover,
            // :pressed, and :focus while it's the active window): a rounded top,
            // a filler squaring its bottom corners, and a line under it.
            create(
                bar,
                NodeKind::Rect,
                json!({"x": 1, "y": 1, "w": fw - 2.0, "h": TITLE_BAR_HEIGHT - 1.0, "radius": 9, "class": "titlebar"}),
            ),
            create(
                fill,
                NodeKind::Rect,
                json!({"x": 1, "y": TITLE_BAR_HEIGHT / 2.0, "w": fw - 2.0, "h": TITLE_BAR_HEIGHT / 2.0, "class": "titlebar"}),
            ),
            create(
                line,
                NodeKind::Rect,
                json!({"x": 1, "y": TITLE_BAR_HEIGHT - 1.0, "w": fw - 2.0, "h": 1, "class": "titlebar-line"}),
            ),
            create(
                title,
                NodeKind::Text,
                json!({"x": 14, "y": 10, "w": (fw - 90.0).max(0.0), "h": 18, "text": params.title, "size": 13, "weight": "bold", "class": "window-title"}),
            ),
            create(
                close,
                NodeKind::Button,
                json!({"x": fw - 34.0, "y": 6, "w": 24, "h": 24, "label": "×", "size": 15}),
            ),
            create(
                max,
                NodeKind::Button,
                json!({"x": fw - 62.0, "y": 6, "w": 24, "h": 24, "label": "□", "size": 13}),
            ),
            create(
                content,
                NodeKind::Group,
                json!({"y": TITLE_BAR_HEIGHT, "w": fw, "h": h as f32, "clip": true}),
            ),
            insert(frame, bg),
            insert(frame, bar),
            insert(frame, fill),
            insert(frame, line),
            insert(frame, title),
            insert(frame, close),
            insert(frame, max),
            insert(frame, content),
            insert(ROOT, frame),
        ];
        scene.apply_all(ops);
        self.targets.insert(close, Target::Close(window));
        self.targets.insert(max, Target::Max(window));
        let parts = [bg, bar, fill, line, title, close, max];
        let win = Window {
            owner,
            request,
            frame,
            content,
            parts,
            size: (w, h),
            restore: None,
            ids: HashMap::new(),
        };
        self.windows.insert(window, win);
        self.by_request.insert(request, window);
        Reply { request, event: WindowEvent::Opened { window, width: w, height: h } }
    }

    /// Apply `commit` from `from`. Nothing is applied unless every op is valid.
    /// Returns the scene ids of the nodes it created.
    pub fn commit(
        &mut self,
        scene: &mut Scene,
        from: &Sender,
        commit: Commit,
    ) -> Result<Vec<NodeId>, String> {
        let Some(w) = self.windows.get(&commit.window) else {
            return Err(format!("no window {}", commit.window));
        };
        if &w.owner != from {
            return Err(format!("window {} belongs to another connection", commit.window));
        }
        let mut ops = Vec::with_capacity(commit.ops.len());
        let mut listens = Vec::new();
        let mut asked = Vec::new();
        let mut result = Ok(());
        let mut created = Vec::new();
        for op in commit.ops {
            match self.translate(commit.window, op) {
                Ok(Translated::Op(mut op)) => {
                    match &mut op {
                        SceneOp::Create { id, .. } => created.push(*id),
                        SceneOp::Set { id, props } => {
                            if let Some(v) = props.get("_value").cloned()
                                && self.is_echo(*id, &v)
                            {
                                props.remove("_value");
                            }
                            if let Some(v) = props.remove("selection") {
                                let at = |i: usize| v.get(i).and_then(Value::as_u64).map(|n| n as usize);
                                match (at(0), at(1)) {
                                    (Some(a), Some(c)) => asked.push((*id, Asked::Select(a, c))),
                                    _ => {
                                        result = Err("selection is [anchor, caret]".to_owned());
                                        break;
                                    }
                                }
                            }
                            if props.remove("focus") == Some(Value::Bool(true)) {
                                asked.push((*id, Asked::Focus));
                            }
                            if let Some(v) = props.remove("undo_group") {
                                let sets: Option<Vec<i64>> =
                                    v.as_array().and_then(|a| a.iter().map(Value::as_i64).collect());
                                match sets {
                                    Some(sets) => asked.push((*id, Asked::Undoable(sets))),
                                    None => {
                                        result = Err("undo_group is a list of change sets".to_owned());
                                        break;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    ops.push(op)
                }
                Ok(Translated::Listen(global, app_id, events)) => listens.push((global, app_id, events)),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if result.is_ok() {
            scene.apply_all(ops);
            let w = &self.windows[&commit.window];
            for (&app_id, &global) in &w.ids {
                self.owners.insert(global, (commit.window, app_id));
            }
            for (global, app_id, events) in listens {
                if events.iter().any(|e| e == "click") {
                    self.targets.insert(global, Target::Node(commit.window, app_id));
                } else {
                    self.targets.remove(&global);
                }
                if events.iter().any(|e| e == "select") {
                    self.selects.insert(global);
                } else {
                    self.selects.remove(&global);
                }
            }
            self.asked.extend(asked);
        }
        // Forget ids whose nodes are gone: destroyed, or allocated by a commit that
        // was refused.
        self.prune(scene, commit.window);
        result.map(|()| created)
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
            SceneOp::Set { id, props } => {
                // An app setting a widget's value replaces what the user typed there.
                let mut p = app_props(props);
                if let Some(v) = p.get("value").cloned() {
                    p.insert("_value".into(), v);
                }
                SceneOp::Set { id: own(w, id)?, props: p }
            }
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
        self.owners.retain(|g, _| scene.node(*g).is_some());
        self.reported.retain(|g, _| scene.node(*g).is_some());
        self.selects.retain(|g| scene.node(*g).is_some());
    }

    /// Whether `node`'s app listens for its `select` events.
    pub fn listens_for_select(&self, node: NodeId) -> bool {
        self.selects.contains(&node)
    }

    /// What apps asked of their widgets since this was last called, in order.
    pub fn take_asked(&mut self) -> Vec<(NodeId, Asked)> {
        std::mem::take(&mut self.asked)
    }

    /// The app's request `request` ended (cancelled, answered, or its connection
    /// closed): remove its window. Returns whether there was one.
    pub fn close_request(&mut self, scene: &mut Scene, request: u64) -> bool {
        let Some(window) = self.by_request.remove(&request) else { return false };
        let w = self.windows.remove(&window).expect("indexes agree");
        scene.apply(SceneOp::Destroy { id: w.frame });
        self.targets.retain(|g, _| scene.node(*g).is_some());
        self.owners.retain(|g, _| scene.node(*g).is_some());
        self.selects.retain(|g| scene.node(*g).is_some());
        if self.pressed.is_some_and(|p| scene.node(p).is_none()) {
            self.pressed = None;
        }
        true
    }

    /// The pointer went down at (`x`, `y`) in root child `window` (the topmost
    /// window there). Returns true if it landed on a node that takes presses, which
    /// is then shown pressed until the pointer comes up.
    pub fn press(&mut self, scene: &mut Scene, window: NodeId, x: f32, y: f32) -> bool {
        self.press_where(scene, window, x, y, |_| true)
    }

    /// Like [`Apps::press`], but only on nodes `allow` accepts (an open dialog
    /// blocks the rest of its window).
    pub fn press_where(
        &mut self,
        scene: &mut Scene,
        window: NodeId,
        x: f32,
        y: f32,
        allow: impl Fn(NodeId) -> bool,
    ) -> bool {
        let Some(node) = scene.hit(window, x, y, |id| self.targets.contains_key(&id) && allow(id)) else {
            return false;
        };
        self.pressed = Some(node);
        scene.apply(SceneOp::Set { id: node, props: props(json!({"pressed": true})) });
        true
    }

    /// Whether scene node `node` is one of an app's own.
    pub fn owns(&self, node: NodeId) -> bool {
        self.owners.contains_key(&node)
    }

    /// Window `window`'s close and maximize buttons.
    pub fn frame_buttons(&self, window: u64) -> Vec<NodeId> {
        self.windows.get(&window).map(|w| vec![w.parts[5], w.parts[6]]).unwrap_or_default()
    }

    /// Whether `node` is a window's close or maximize button.
    pub fn is_frame_button(&self, node: NodeId) -> bool {
        matches!(self.targets.get(&node), Some(Target::Close(_) | Target::Max(_)))
    }

    /// A click on `node` from the keyboard: the app's event if it listens for clicks.
    pub fn click(&self, node: NodeId) -> Option<Reply> {
        match self.targets.get(&node)? {
            Target::Node(..) => self.event(node, "click", None),
            _ => None,
        }
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
            Target::Node(w, _) | Target::Close(w) | Target::Max(w) => w,
        };
        let w = self.windows.get(&window)?;
        // Dragged off before letting go: no click.
        scene.hit(w.frame, x, y, |id| id == node)?;
        let event = match target {
            Target::Node(_, app_id) => {
                WindowEvent::Event { node: app_id, event: "click".into(), value: None }
            }
            Target::Close(_) => WindowEvent::Close,
            Target::Max(window) => return self.toggle_maximize(scene, window),
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

/// An app's props, minus the ones only the compositor may set: `window`,
/// `workspace`, `pressed`, and everything starting with `_` (widget state).
fn app_props(mut p: Props) -> Props {
    p.retain(|k, _| !matches!(k.as_str(), "window" | "workspace" | "pressed") && !k.starts_with('_'));
    p
}

fn set(id: NodeId, v: Value) -> SceneOp {
    SceneOp::Set { id, props: props(v) }
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
        let params = OpenWindowParams {
            title: "Counter".into(),
            width: Some(300),
            height: Some(200),
            style: None,
            app: None,
        };
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

    /// Lay out the scene, as the desktop does before drawing or hit-testing.
    fn lay(scene: &mut Scene) {
        crate::layout::Layout::new(crate::fonts::ui_only()).run(scene);
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
        lay(&mut scene);

        // The stack laid the button out below the label, inside the content area.
        let (bx, by) = find_button(&scene, &apps, w, 4);
        let frame = apps.windows[&w].frame;
        assert!(apps.press(&mut scene, frame, bx, by));
        let pressed = apps.windows[&w].ids[&4];
        assert_eq!(scene.node(pressed).unwrap().props["pressed"], true);
        let reply = apps.release(&mut scene, bx, by).unwrap();
        assert_eq!(
            reply,
            Reply { request: 77, event: WindowEvent::Event { node: 4, event: "click".into(), value: None } }
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
        lay(&mut scene);
        let frame = apps.windows[&w].frame;
        let r = scene.local_rect(frame).unwrap();
        let (cx, cy) = (r.x + r.w - 22.0, r.y + 18.0);
        assert!(apps.press(&mut scene, frame, cx, cy));
        assert_eq!(apps.release(&mut scene, cx, cy), Some(Reply { request: 9, event: WindowEvent::Close }));
        assert!(scene.node(frame).is_some(), "the app decides");

        let before = scene.len();
        assert!(apps.close_request(&mut scene, 9));
        assert!(scene.node(frame).is_none());
        assert_eq!(before - scene.len(), 9 + 3, "the frame's nine nodes and the app's three");
        assert!(apps.is_empty() && apps.targets.is_empty() && apps.owners.is_empty());
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
        assert_eq!(apps.targets.len(), 2, "only the close and maximize buttons");
        // The id can be used again.
        apps.commit(&mut scene, &app(5), Commit { window: w, ops: counter_ui() }).unwrap();
        assert_eq!(text_of(&scene, &apps, w, 3), "Count: 0");
    }

    #[test]
    fn new_windows_go_side_by_side_then_in_rows_then_cascade() {
        let mut scene = Scene::new();
        let mut apps = Apps::new();
        let spot = |apps: &mut Apps, scene: &mut Scene, n: u64| {
            let p = OpenWindowParams {
                title: "w".into(),
                width: Some(300),
                height: Some(200),
                style: None,
                app: None,
            };
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
    fn windows_resize_and_maximize_and_tell_the_app() {
        let mut scene = Scene::new();
        let mut apps = Apps::new();
        let w = open(&mut apps, &mut scene, app(5), 3);
        apps.commit(&mut scene, &app(5), Commit { window: w, ops: counter_ui() }).unwrap();
        let reply = apps.resize(&mut scene, w, 500, 320).unwrap();
        assert_eq!(reply, Reply { request: 3, event: WindowEvent::Resized { width: 500, height: 320 } });
        assert!(apps.resize(&mut scene, w, 500, 320).is_none(), "no change, no event");
        let frame = apps.windows[&w].frame;
        assert_eq!(scene.local_rect(frame).unwrap().w, 500.0);
        // The content reflows: the app's stack fills the new size.
        lay(&mut scene);
        let stack = apps.windows[&w].ids[&2];
        assert_eq!(scene.layout_rect(stack).unwrap().w, 500.0);

        apps.set_work_area(0.0, 30.0, 1280.0, 770.0);
        let before = scene.local_rect(frame).unwrap();
        let max = apps.toggle_maximize(&mut scene, w).unwrap();
        assert_eq!(max.event, WindowEvent::Resized { width: 1280, height: 770 - 36 });
        assert_eq!((scene.local_rect(frame).unwrap().x, scene.local_rect(frame).unwrap().y), (0.0, 30.0));
        apps.toggle_maximize(&mut scene, w).unwrap();
        assert_eq!(scene.local_rect(frame).unwrap(), before, "put back where it was");
    }

    #[test]
    fn widget_events_go_to_the_owning_app_with_its_id() {
        let mut scene = Scene::new();
        let mut apps = Apps::new();
        let w = open(&mut apps, &mut scene, app(5), 8);
        let created = apps.commit(&mut scene, &app(5), Commit { window: w, ops: counter_ui() }).unwrap();
        assert_eq!(created.len(), 3);
        let label = apps.windows[&w].ids[&3];
        let reply = apps.event(label, "change", Some(json!(true))).unwrap();
        assert_eq!(
            reply,
            Reply {
                request: 8,
                event: WindowEvent::Event { node: 3, event: "change".into(), value: Some(json!(true)) }
            }
        );
        assert!(apps.event(apps.windows[&w].frame, "change", None).is_none(), "frame parts aren't the app's");
        // Apps can't set the compositor's widget state.
        let sneaky =
            ops(json!([{"op": "set", "id": 3, "props": {"_value": "x", "_focus": true, "workspace": 3}}]));
        apps.commit(&mut scene, &app(5), Commit { window: w, ops: sneaky }).unwrap();
        let p = &scene.node(label).unwrap().props;
        assert!(p.get("_value").is_none() && p.get("_focus").is_none() && p.get("workspace").is_none());
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
