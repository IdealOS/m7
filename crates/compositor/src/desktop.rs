//! The desktop: the scene, layout, window manager, pointer and keyboard, app
//! windows, widgets, the shell and the clipboard, driven by input from a backend
//! and by requests from the bus.
//!
//! Both backends own one and feed it; it says what to send back over the bus.
//! Widgets are driven here, in the compositor, without a round trip to the app
//! (the "thick compositor" rule): typing, caret movement, sliders, scrolling and
//! menus happen immediately, and the app hears the results as events (`change`
//! with a value, `submit`, `range`). A text field bound to a document edits it
//! through `dbd` directly. Keeping all of this free of I/O means the whole path
//! from a click or a key to the scene and the app is tested without a display.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use glyphon::FontSystem;
use protocol::bus::code;
use protocol::perm::{AppGrants, GrantsResult, capability};
use protocol::scene::{PromptParams, method};
use protocol::{BusError, Clipboard, Commit, NodeId, NodeKind, OpenWindowParams, Props, SceneOp, Sender};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::apps::{Apps, Reply};
use crate::demo::TOP_BAR;
use crate::draw::FIELD_PAD;
use crate::input::{Effect, Input, Key, Mods, Seat, Shortcut};
use crate::layout::{DIVIDER, Layout};
use crate::prompt;
use crate::scene::{ROOT, Rect, Scene, num};
use crate::shell::{self, BAR_HEIGHT, WORKSPACES};
use crate::textedit::{Edit, Move, Splice};
use crate::theme;
use crate::wm::{self, WindowManager};

/// A request (or a cancel) addressed to the compositor, or the answer to one the
/// compositor sent.
#[derive(Debug)]
pub enum Call {
    Request {
        id: u64,
        from: Option<Sender>,
        method: String,
        params: Value,
        stream: bool,
    },
    Cancel {
        id: u64,
    },
    /// The answer to [`Out::Request`] `token`.
    Reply {
        token: u64,
        result: Result<Value, BusError>,
    },
    /// The bytes of blob `hash`, asked for with [`Out::FetchBlob`] for `app`.
    Blob {
        hash: String,
        app: Option<String>,
        result: Result<Vec<u8>, String>,
    },
    /// permd published a new grant table.
    Grants(GrantsResult),
}

/// What to send over the bus.
#[derive(Debug, PartialEq)]
pub enum Out {
    /// A part of a streamed answer (a window event).
    Part { id: u64, data: Value },
    /// The answer to request `id`, ending it.
    Respond { id: u64, result: Result<Value, BusError> },
    /// A request of the compositor's own (loading a bound field, listing apps,
    /// launching one). Its answer comes back as [`Call::Reply`] with `token`; 0
    /// means the answer isn't needed.
    Request { token: u64, to: String, method: String, params: Value },
    /// Read blob `hash` (an image) from `db`, as app `app` if given; it comes
    /// back as [`Call::Blob`].
    FetchBlob { hash: String, app: Option<String> },
}

/// Most bytes of decoded images kept that no image node shows. Shown ones are
/// always kept (each is at most 4096 by 4096).
pub const IMAGE_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// A decoded image, and when it was last wanted, for evicting.
struct Cached {
    image: Option<std::sync::Arc<crate::image::Image>>,
    used: u64,
}

/// What an answer to one of the compositor's requests is for.
enum Pending {
    /// The document behind a bound text field.
    Bind(NodeId),
    /// The installed apps, for the launcher menu.
    Launcher,
}

/// A pointer interaction in progress.
enum Drag {
    Slider(NodeId),
    /// Selecting text with the pointer.
    Text(NodeId),
    Split(NodeId),
    /// Dragging content to scroll it; `velocity` (pixels per second) carries on
    /// when it's let go.
    Scroll {
        node: NodeId,
        last: f32,
        velocity: f32,
    },
    Resize {
        window: u64,
        from: (f32, f32),
        size: (u32, u32),
        last: Option<Reply>,
    },
}

/// An open popup menu.
struct Menu {
    labels: Vec<String>,
    values: Vec<String>,
    highlight: usize,
    rect: Rect,
    purpose: MenuFor,
}

#[derive(Clone, Copy)]
enum MenuFor {
    Select(NodeId),
    Menu(NodeId),
    Launcher,
}

/// Whether `s` looks like a blob's name: 64 lowercase hex digits (SHA-256).
fn is_blob_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Scene ids the desktop itself uses for overlays, above every other range.
const MENU: NodeId = 1 << 40;
const SWITCHER: NodeId = (1 << 40) + 10_000;
const ROW: f32 = 28.0;
/// How long the window switcher stays up after Alt+Tab.
const SWITCHER_SHOWS: Duration = Duration::from_millis(1200);
/// The corner of an app window that resizes it.
const GRIP: f32 = 14.0;

pub struct Desktop {
    pub scene: Scene,
    pub wm: WindowManager,
    pub seat: Seat,
    pub apps: Apps,
    pub layout: Layout,
    pub clipboard: Clipboard,
    pub workspace: u32,
    focus: Option<NodeId>,
    edits: HashMap<NodeId, Edit>,
    drag: Option<Drag>,
    /// Scroll views still moving after a drag, with their speed.
    coasting: Vec<(NodeId, f32)>,
    last_tick: Option<Instant>,
    menu: Option<Menu>,
    switcher_until: Option<Instant>,
    pending: HashMap<u64, Pending>,
    next_token: u64,
    outbox: Vec<Out>,
    /// Decoded images by blob hash; `None` while one is loading or if it failed.
    images: HashMap<String, Cached>,
    /// Which apps were allowed which blobs (`None` is a system window). An image
    /// node shows its blob only once its app was.
    blob_ok: std::collections::HashSet<(Option<String>, String)>,
    /// Blob fetches in flight, by who they're for.
    fetching: std::collections::HashSet<(Option<String>, String)>,
    uses: u64,
    /// Every app's grants, as permd last published them.
    pub grants: HashMap<String, AppGrants>,
    grants_version: i64,
    /// Permission prompts waiting, by request id; the first is shown.
    prompts: std::collections::VecDeque<(u64, PromptParams)>,
    shown_prompt: Option<prompt::Shown>,
    allow_focused: bool,
}

impl Desktop {
    /// An empty desktop for a `w` by `h` screen, measuring text with the system's
    /// fonts plus the bundled UI font.
    pub fn new(w: f32, h: f32) -> Desktop {
        Desktop::with_fonts(w, h, crate::fonts::system())
    }

    /// Like [`Desktop::new`] with the given fonts (tests use the bundled font only).
    pub fn with_fonts(w: f32, h: f32, fonts: FontSystem) -> Desktop {
        let mut apps = Apps::new();
        apps.set_work_area(0.0, BAR_HEIGHT, w, h - BAR_HEIGHT);
        Desktop {
            scene: Scene::new(),
            wm: WindowManager::default(),
            seat: Seat::new(w, h),
            apps,
            layout: Layout::new(fonts),
            clipboard: Clipboard::default(),
            workspace: 1,
            focus: None,
            edits: HashMap::new(),
            drag: None,
            coasting: Vec::new(),
            last_tick: None,
            menu: None,
            switcher_until: None,
            pending: HashMap::new(),
            next_token: 1,
            outbox: Vec::new(),
            images: HashMap::new(),
            blob_ok: Default::default(),
            fetching: Default::default(),
            uses: 0,
            grants: HashMap::new(),
            grants_version: -1,
            prompts: Default::default(),
            shown_prompt: None,
            allow_focused: false,
        }
    }

    /// The screen is `w` by `h` (at startup, or after a resize): keep the pointer,
    /// maximized windows and the shell in step.
    pub fn set_screen(&mut self, w: f32, h: f32) {
        self.seat.set_bounds(w, h);
        self.apps.set_work_area(0.0, BAR_HEIGHT, w, h - BAR_HEIGHT);
        shell::install(&mut self.scene, w, self.workspace);
        shell::set_clock(&mut self.scene, &shell::clock_text(SystemTime::now()));
        self.show_prompt();
    }

    fn drain(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.outbox)
    }

    fn send(&mut self, reply: Option<Reply>) {
        if let Some(r) = reply {
            self.outbox.push(part(r));
        }
    }

    fn event(&mut self, node: NodeId, event: &str, value: Value) {
        let reply = self.apps.event(node, event, Some(value));
        self.send(reply);
    }

    fn request(&mut self, pending: Option<Pending>, to: &str, method: &str, params: Value) {
        let token = match pending {
            Some(p) => {
                let t = self.next_token;
                self.next_token += 1;
                self.pending.insert(t, p);
                t
            }
            None => 0,
        };
        self.outbox.push(Out::Request { token, to: to.into(), method: method.into(), params });
    }

    fn set(&mut self, id: NodeId, v: Value) {
        self.scene.apply(SceneOp::Set { id, props: props(v) });
    }

    fn prop<'a>(&'a self, id: NodeId, key: &str) -> Option<&'a Value> {
        self.scene.node(id)?.props.get(key)
    }

    fn kind(&self, id: NodeId) -> Option<NodeKind> {
        self.scene.node(id).map(|n| n.kind)
    }

    /// Lay out the scene if it changed, and update what depends on layout:
    /// scroll extents, split dividers, virtual lists' visible rows.
    pub fn relayout(&mut self) {
        if !self.scene.needs_layout() {
            return;
        }
        self.layout.run(&mut self.scene);
        let ids: Vec<(NodeId, NodeKind)> = self
            .scene
            .ids()
            .filter_map(|id| self.kind(id).map(|k| (id, k)))
            .filter(|(_, k)| matches!(k, NodeKind::Scroll | NodeKind::Split | NodeKind::Vlist))
            .collect();
        for (id, kind) in ids {
            match kind {
                NodeKind::Scroll => {
                    let content = self.scene.scroll_content(id).map_or(0.0, |c| c.1);
                    if self.prop(id, "_content").and_then(Value::as_f64) != Some(content as f64) {
                        self.set(id, json!({"_content": content}));
                        self.scroll_by(id, 0.0);
                    }
                }
                NodeKind::Split => {
                    let vertical = self.prop(id, "vertical").and_then(Value::as_bool) == Some(true);
                    let first = self.scene.node(id).and_then(|n| n.children.first().copied());
                    if let Some(r) = first.and_then(|c| self.scene.layout_rect(c)) {
                        let at = if vertical { r.y + r.h } else { r.x + r.w };
                        if self.prop(id, "_divider").and_then(Value::as_f64) != Some(at as f64) {
                            self.set(id, json!({"_divider": at}));
                        }
                    }
                }
                _ => self.vlist_range(id),
            }
        }
    }

    /// The scene, laid out, as a draw list, with images' pixels attached.
    pub fn draw_list(&mut self) -> crate::scene::DrawList {
        self.relayout();
        let mut list = self.scene.draw_list();
        for item in list.layers.iter_mut().flat_map(|l| l.images.iter_mut()) {
            item.pixels = self.images.get(&item.blob).and_then(|c| c.image.clone());
        }
        list
    }

    // --- Input -------------------------------------------------------------------

    /// Apply one input. Returns what the backend should act on, and what to send.
    pub fn input(&mut self, input: Input) -> (Effect, Vec<Out>) {
        self.relayout();
        if !self.prompts.is_empty() {
            return (self.prompt_input(input), self.drain());
        }
        let effect = match input {
            Input::Key { key, mods } => self.key(key, mods),
            Input::Scroll { dy, .. } => {
                self.wheel(dy);
                Effect::default()
            }
            Input::Button { pressed: true } => self.press(),
            Input::Button { pressed: false } => self.release(),
            Input::Motion { .. } | Input::MotionTo { .. } => {
                let effect = self.seat.handle(input, &mut self.scene, &mut self.wm);
                self.motion();
                effect
            }
            Input::Shortcut(_) => self.seat.handle(input, &mut self.scene, &mut self.wm),
        };
        (effect, self.drain())
    }

    fn press(&mut self) -> Effect {
        let (x, y) = self.seat.cursor();
        if self.menu.is_some() {
            self.menu_press(x, y);
            return Effect::default();
        }
        if let Some(action) = shell::press(&self.scene, x, y) {
            match action {
                shell::Action::Launcher => {
                    self.request(
                        Some(Pending::Launcher),
                        "db",
                        "query",
                        json!({"type": "App", "limit": 100}),
                    );
                }
                shell::Action::Workspace(n) => self.switch_workspace(n),
            }
            return Effect::default();
        }
        let Some(window) = self.wm.window_at(&self.scene, x, y) else {
            self.set_focus(None);
            return Effect::default();
        };
        self.scene.raise(window);
        self.restack();
        let dialog = self.dialog_in(window);
        // With a dialog open, only its own nodes and the frame's buttons take presses.
        let allowed: Option<HashSet<NodeId>> = dialog.map(|d| {
            let mut ids: HashSet<NodeId> = self.scene.paint_order(d).into_iter().map(|(id, ..)| id).collect();
            if let Some(win) = self.apps.window_of(window) {
                ids.extend(self.apps.frame_buttons(win));
            }
            ids
        });
        let allow = |id: NodeId| allowed.as_ref().is_none_or(|ids| ids.contains(&id));
        if self.apps.press_where(&mut self.scene, window, x, y, allow) {
            if let Some(n) = self.apps_pressed_node(window, x, y)
                && self.apps.owns(n)
            {
                self.set_focus(Some(n));
            }
            return Effect::default();
        }
        // The bottom-right corner resizes an app window.
        if let Some(win) = self.apps.window_of(window)
            && let Some(r) = self.scene.local_rect(window)
            && x >= r.x + r.w - GRIP
            && y >= r.y + r.h - GRIP
            && let Some(size) = self.apps.size_of(win)
        {
            self.drag = Some(Drag::Resize { window: win, from: (x, y), size, last: None });
            return Effect::default();
        }
        let root = dialog.unwrap_or(window);
        if let Some((node, kind, rect)) = self.widget_at(root, x, y) {
            match kind {
                NodeKind::Checkbox | NodeKind::Toggle => {
                    self.set_focus(Some(node));
                    self.toggle(node);
                }
                NodeKind::Slider => {
                    self.set_focus(Some(node));
                    self.drag = Some(Drag::Slider(node));
                    self.slide_to(node, rect, x);
                }
                NodeKind::Select => {
                    self.set_focus(Some(node));
                    self.open_select(node, rect);
                }
                NodeKind::Menu => {
                    self.set_focus(Some(node));
                    self.open_actions(node, rect);
                }
                NodeKind::TextField | NodeKind::TextArea => {
                    self.set_focus(Some(node));
                    let at = self.index_at(node, rect, x, y);
                    self.edit(node, |e| {
                        e.move_caret(Move::To(at), false);
                        None
                    });
                    self.drag = Some(Drag::Text(node));
                }
                NodeKind::Split => self.drag = Some(Drag::Split(node)),
                NodeKind::Scroll | NodeKind::Vlist => {
                    self.coasting.retain(|(n, _)| *n != node);
                    self.drag = Some(Drag::Scroll { node, last: y, velocity: 0.0 });
                }
                _ => {}
            }
            return Effect::default();
        }
        if dialog.is_some() && y > self.scene.local_rect(window).map_or(0.0, |r| r.y + wm::TITLE_BAR_HEIGHT) {
            // Clicks outside an open dialog's panel do nothing.
            return Effect::default();
        }
        self.set_focus(None);
        self.seat.handle(Input::Button { pressed: true }, &mut self.scene, &mut self.wm)
    }

    /// The node `Apps` just marked pressed (for focus), found the same way it did.
    fn apps_pressed_node(&self, window: NodeId, x: f32, y: f32) -> Option<NodeId> {
        self.scene.hit(window, x, y, |id| {
            self.scene.node(id).and_then(|n| n.props.get("pressed")).and_then(Value::as_bool) == Some(true)
        })
    }

    fn release(&mut self) -> Effect {
        let (x, y) = self.seat.cursor();
        if self.apps.pressing() {
            let reply = self.apps.release(&mut self.scene, x, y);
            self.send(reply);
            return Effect::default();
        }
        match self.drag.take() {
            Some(Drag::Scroll { node, velocity, .. }) => {
                if velocity.abs() > 60.0 {
                    self.coasting.push((node, velocity));
                }
                Effect::default()
            }
            Some(Drag::Resize { last, .. }) => {
                // The app hears the final size once, at the end of the drag.
                self.send(last);
                Effect::default()
            }
            Some(_) => Effect::default(),
            None => self.seat.handle(Input::Button { pressed: false }, &mut self.scene, &mut self.wm),
        }
    }

    fn motion(&mut self) {
        let (x, y) = self.seat.cursor();
        let Some(drag) = self.drag.take() else { return };
        let rects = |s: &Self, n: NodeId| s.scene.screen_rects().get(&n).map(|r| r.0);
        self.drag = Some(match drag {
            Drag::Slider(n) => {
                if let Some(r) = rects(self, n) {
                    self.slide_to(n, r, x);
                }
                Drag::Slider(n)
            }
            Drag::Text(n) => {
                if let Some(r) = rects(self, n) {
                    let at = self.index_at(n, r, x, y);
                    self.edit(n, |e| {
                        e.move_caret(Move::To(at), true);
                        None
                    });
                }
                Drag::Text(n)
            }
            Drag::Split(n) => {
                if let Some(r) = rects(self, n) {
                    let vertical = self.prop(n, "vertical").and_then(Value::as_bool) == Some(true);
                    let ratio = if vertical { (y - r.y) / r.h } else { (x - r.x) / r.w };
                    let ratio = (ratio * 1000.0).round() / 1000.0;
                    let ratio = ratio.clamp(0.1, 0.9);
                    self.set(n, json!({"ratio": ratio}));
                    self.event(n, "change", json!(ratio));
                }
                Drag::Split(n)
            }
            Drag::Scroll { node, last, .. } => {
                let dy = last - y;
                self.scroll_by(node, dy);
                Drag::Scroll { node, last: y, velocity: dy * 60.0 }
            }
            Drag::Resize { window, from, size, last } => {
                let w = (size.0 as f32 + x - from.0).max(0.0) as u32;
                let h = (size.1 as f32 + y - from.1).max(0.0) as u32;
                let reply = self.apps.resize(&mut self.scene, window, w, h).or(last);
                Drag::Resize { window, from, size, last: reply }
            }
        });
    }

    /// Scroll whatever scrolls under the pointer, in the topmost window there.
    fn wheel(&mut self, dy: f32) {
        let (x, y) = self.seat.cursor();
        let Some(window) = self.wm.window_at(&self.scene, x, y) else { return };
        let root = self.dialog_in(window).unwrap_or(window);
        for (id, rect, clip) in self.scene.paint_order(root).into_iter().rev() {
            if !rect.intersect(&clip).contains(x, y) {
                continue;
            }
            if matches!(self.kind(id), Some(NodeKind::Scroll | NodeKind::Vlist | NodeKind::TextArea))
                && self.scroll_extent(id) > 0.0
            {
                self.scroll_by(id, dy);
                return;
            }
        }
    }

    /// How far node `id` can scroll.
    fn scroll_extent(&mut self, id: NodeId) -> f32 {
        let Some(view) = self.scene.layout_rect(id).or_else(|| self.scene.local_rect(id)) else { return 0.0 };
        let content = match self.kind(id) {
            Some(NodeKind::Scroll) => num(&self.scene.node(id).expect("exists").props, "_content", 0.0),
            Some(NodeKind::Vlist) => {
                let p = &self.scene.node(id).expect("exists").props;
                num(p, "count", 0.0) * num(p, "row_height", 28.0)
            }
            Some(NodeKind::TextArea) => {
                let p = &self.scene.node(id).expect("exists").props;
                let text =
                    p.get("_value").or(p.get("value")).and_then(Value::as_str).unwrap_or_default().to_owned();
                let size = num(p, "size", theme::size::BODY);
                self.layout.text_extent(&text, size, Some((view.w - 2.0 * FIELD_PAD).max(1.0))).1 + 12.0
            }
            _ => 0.0,
        };
        (content - view.h).max(0.0)
    }

    fn scroll_by(&mut self, id: NodeId, dy: f32) {
        let extent = self.scroll_extent(id);
        let now = self.prop(id, "_scroll").and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let to = (now + dy).clamp(0.0, extent);
        if to != now {
            self.set(id, json!({"_scroll": to}));
        }
        if self.kind(id) == Some(NodeKind::Vlist) {
            self.vlist_range(id);
        }
    }

    /// Tell a virtual list's app which rows are visible, when that changes.
    fn vlist_range(&mut self, id: NodeId) {
        let Some(view) = self.scene.layout_rect(id).or_else(|| self.scene.local_rect(id)) else { return };
        let p = &self.scene.node(id).expect("exists").props;
        let (count, row) = (num(p, "count", 0.0) as i64, num(p, "row_height", 28.0).max(1.0));
        let scrolled = num(p, "_scroll", 0.0);
        if count == 0 || view.h <= 0.0 {
            return;
        }
        let first = ((scrolled / row).floor() as i64).clamp(0, count - 1);
        let last = (((scrolled + view.h) / row).ceil() as i64 - 1).clamp(first, count - 1);
        let range = json!([first, last]);
        if p.get("_range") != Some(&range) {
            self.set(id, json!({"_range": range.clone()}));
            self.event(id, "range", range);
        }
    }

    /// The interactive widget under (`x`, `y`) in `root`, topmost first.
    fn widget_at(&mut self, root: NodeId, x: f32, y: f32) -> Option<(NodeId, NodeKind, Rect)> {
        for (id, rect, clip) in self.scene.paint_order(root).into_iter().rev() {
            if !rect.intersect(&clip).contains(x, y) {
                continue;
            }
            let Some(kind) = self.kind(id) else { continue };
            match kind {
                NodeKind::Checkbox
                | NodeKind::Toggle
                | NodeKind::Slider
                | NodeKind::Select
                | NodeKind::Menu
                | NodeKind::TextField
                | NodeKind::TextArea => return Some((id, kind, rect)),
                NodeKind::Split => {
                    let p = &self.scene.node(id).expect("exists").props;
                    let at = num(p, "_divider", -1.0);
                    let vertical = p.get("vertical").and_then(Value::as_bool) == Some(true);
                    let (pos, start) = if vertical { (y, rect.y) } else { (x, rect.x) };
                    if at >= 0.0 && pos >= start + at - 2.0 && pos <= start + at + DIVIDER + 2.0 {
                        return Some((id, kind, rect));
                    }
                }
                NodeKind::Scroll | NodeKind::Vlist if self.scroll_extent(id) > 0.0 => {
                    return Some((id, kind, rect));
                }
                _ => {}
            }
        }
        None
    }

    /// The open dialog in window `window`, if any.
    fn dialog_in(&self, window: NodeId) -> Option<NodeId> {
        self.scene
            .paint_order(window)
            .into_iter()
            .map(|(id, ..)| id)
            .filter(|&id| self.kind(id) == Some(NodeKind::Dialog))
            .last()
    }

    /// Keep the top bar and overlays above every window.
    fn restack(&mut self) {
        for id in [TOP_BAR, MENU, SWITCHER, prompt::PROMPT] {
            if self.scene.node(id).is_some() {
                self.scene.raise(id);
            }
        }
    }

    // --- Widgets -----------------------------------------------------------------

    fn toggle(&mut self, node: NodeId) {
        let on = self.prop(node, "checked").and_then(Value::as_bool) != Some(true);
        self.set(node, json!({"checked": on}));
        self.event(node, "change", json!(on));
    }

    fn slide_to(&mut self, node: NodeId, rect: Rect, x: f32) {
        let p = &self.scene.node(node).expect("exists").props;
        let (min, max, step) = (num(p, "min", 0.0), num(p, "max", 100.0), num(p, "step", 1.0));
        let t = ((x - rect.x - 8.0) / (rect.w - 16.0).max(1.0)).clamp(0.0, 1.0);
        self.set_slider(node, min + t * (max - min), min, max, step);
    }

    fn set_slider(&mut self, node: NodeId, value: f32, min: f32, max: f32, step: f32) {
        let mut v = value.clamp(min, max);
        if step > 0.0 {
            v = min + ((v - min) / step).round() * step;
        }
        let v = (v.clamp(min, max) * 1000.0).round() / 1000.0;
        if self.prop(node, "value").and_then(Value::as_f64) != Some(v as f64) {
            self.set(node, json!({"value": v}));
            self.event(node, "change", json!(v));
        }
    }

    fn set_focus(&mut self, node: Option<NodeId>) {
        if node == self.focus {
            return;
        }
        if let Some(old) = self.focus.take()
            && self.scene.node(old).is_some()
        {
            self.set(old, json!({"_focus": false, "_sel": Value::Null}));
        }
        self.focus = node;
        if let Some(n) = node {
            self.set(n, json!({"_focus": true}));
            if matches!(self.kind(n), Some(NodeKind::TextField | NodeKind::TextArea)) {
                self.edit(n, |_| None);
            }
        }
    }

    /// Widgets in `root` that take keyboard focus, in order.
    fn focusable(&self, root: NodeId) -> Vec<NodeId> {
        self.scene
            .paint_order(root)
            .into_iter()
            .map(|(id, ..)| id)
            .filter(|&id| {
                self.apps.owns(id)
                    && matches!(
                        self.kind(id),
                        Some(
                            NodeKind::Button
                                | NodeKind::Checkbox
                                | NodeKind::Toggle
                                | NodeKind::Slider
                                | NodeKind::Select
                                | NodeKind::Menu
                                | NodeKind::TextField
                                | NodeKind::TextArea
                        )
                    )
            })
            .collect()
    }

    /// The topmost visible window.
    fn top_window(&self) -> Option<NodeId> {
        self.scene.top_level().iter().rev().copied().find(|&id| wm::is_window(&self.scene, id))
    }

    fn move_focus(&mut self, back: bool) {
        let Some(window) = self.top_window() else { return };
        let root = self.dialog_in(window).unwrap_or(window);
        let order = self.focusable(root);
        if order.is_empty() {
            return;
        }
        let at = self.focus.and_then(|f| order.iter().position(|&n| n == f));
        let next = match (at, back) {
            (None, false) => 0,
            (None, true) => order.len() - 1,
            (Some(i), false) => (i + 1) % order.len(),
            (Some(i), true) => (i + order.len() - 1) % order.len(),
        };
        self.set_focus(Some(order[next]));
    }

    // --- Text --------------------------------------------------------------------

    fn text_value(&self, node: NodeId) -> String {
        let p = &self.scene.node(node).expect("exists").props;
        p.get("_value").or(p.get("value")).and_then(Value::as_str).unwrap_or_default().to_owned()
    }

    /// Change text widget `node` with `f`. A change updates the shown text, tells
    /// the app (`change`), goes to the bound document if there is one, and moves
    /// the caret and selection on screen.
    fn edit(&mut self, node: NodeId, f: impl FnOnce(&mut Edit) -> Option<Splice>) {
        // Start from the shown text, or catch up with it if the app replaced it.
        let shown = self.text_value(node);
        match self.edits.get_mut(&node) {
            Some(e) if e.text() != shown => e.set_text(&shown),
            Some(_) => {}
            None => {
                self.edits.insert(node, Edit::new(&shown));
            }
        }
        let e = self.edits.get_mut(&node).expect("inserted");
        let splice = f(e);
        let text = e.text().to_owned();
        if let Some(s) = splice {
            self.set(node, json!({"_value": text.clone()}));
            self.apps.reported(node, &text);
            self.event(node, "change", json!(text));
            if let Some(Value::Object(bind)) = self.prop(node, "bind").cloned()
                && let (Some(doc), Some(path)) =
                    (bind.get("doc").and_then(Value::as_str), bind.get("path").and_then(Value::as_str))
            {
                let mut params =
                    json!({"id": doc, "path": path, "at": s.at, "delete": s.delete, "insert": s.insert});
                self.act_for(node, &mut params);
                self.request(None, "db", "splice", params);
            }
        }
        self.show_caret(node);
    }

    /// Put text widget `node`'s caret and selection on screen, scrolling to keep
    /// the caret in view.
    fn show_caret(&mut self, node: NodeId) {
        let Some(e) = self.edits.get(&node).cloned() else { return };
        let Some(rect) = self.scene.layout_rect(node).or_else(|| self.scene.local_rect(node)) else { return };
        let multiline = self.kind(node) == Some(NodeKind::TextArea);
        let size = num(&self.scene.node(node).expect("exists").props, "size", theme::size::BODY);
        let inner_w = (rect.w - 2.0 * FIELD_PAD).max(1.0);
        let width = multiline.then_some(inner_w);
        let (cx, cy, ch) = self.layout.caret_at(e.text(), size, width, e.caret());
        let sel: Vec<Value> = e
            .selection()
            .map(|(a, b)| self.layout.selection_rects(e.text(), size, width, a, b))
            .unwrap_or_default()
            .into_iter()
            .map(|r| json!([r.x, r.y, r.w, r.h]))
            .collect();
        let mut changes = json!({"_caret": [cx, cy, ch], "_sel": sel});
        if multiline {
            let inner_h = (rect.h - 8.0).max(1.0);
            let scrolled = num(&self.scene.node(node).expect("exists").props, "_scroll", 0.0);
            let to = if cy < scrolled {
                cy
            } else if cy + ch > scrolled + inner_h {
                cy + ch - inner_h
            } else {
                scrolled
            };
            changes["_scroll"] = json!(to.max(0.0));
        } else {
            let scrolled = num(&self.scene.node(node).expect("exists").props, "_scroll_x", 0.0);
            let to = if cx < scrolled {
                cx
            } else if cx > scrolled + inner_w {
                cx - inner_w
            } else {
                scrolled
            };
            changes["_scroll_x"] = json!(to.max(0.0));
        }
        self.set(node, changes);
    }

    /// The character under (`x`, `y`) in text widget `node`, which is at `rect`.
    fn index_at(&mut self, node: NodeId, rect: Rect, x: f32, y: f32) -> usize {
        let text = self.text_value(node);
        let p = &self.scene.node(node).expect("exists").props;
        let size = num(p, "size", theme::size::BODY);
        let multiline = self.kind(node) == Some(NodeKind::TextArea);
        let inner_x = rect.x + FIELD_PAD;
        if multiline {
            let top = rect.y + 4.0 + 2.0 - num(p, "_scroll", 0.0);
            let width = (rect.w - 2.0 * FIELD_PAD).max(1.0);
            self.layout.index_at(&text, size, Some(width), x - inner_x, y - top)
        } else {
            let scrolled = num(p, "_scroll_x", 0.0);
            self.layout.index_at(&text, size, None, x - inner_x + scrolled, 1.0)
        }
    }

    fn text_key(&mut self, node: NodeId, key: Key, mods: Mods) {
        let multiline = self.kind(node) == Some(NodeKind::TextArea);
        let extend = mods.shift;
        let word = mods.alt || (mods.ctrl && !mods.logo);
        match key {
            Key::Char(c) if mods.primary() => match c.to_ascii_lowercase() {
                'a' => self.edit(node, |e| {
                    e.select_all();
                    None
                }),
                'c' => {
                    let text = self.edits.get(&node).map(Edit::selected_text).unwrap_or_default();
                    if !text.is_empty() {
                        self.clipboard = Clipboard { text, doc: None };
                    }
                }
                'x' => {
                    let mut cut = None;
                    self.edit(node, |e| {
                        let (text, splice) = e.cut()?;
                        cut = Some(text);
                        Some(splice)
                    });
                    if let Some(text) = cut {
                        self.clipboard = Clipboard { text, doc: None };
                    }
                }
                'v' => {
                    let mut text = self.clipboard.text.clone();
                    if !multiline {
                        text = text.replace(['\n', '\r'], " ");
                    }
                    if !text.is_empty() {
                        self.edit(node, |e| Some(e.insert(&text)));
                    }
                }
                _ => {}
            },
            Key::Char(c) => self.edit(node, |e| Some(e.insert(&c.to_string()))),
            Key::Enter if multiline => self.edit(node, |e| Some(e.insert("\n"))),
            Key::Enter => {
                let text = self.text_value(node);
                self.event(node, "submit", json!(text));
            }
            Key::Backspace => self.edit(node, Edit::backspace),
            Key::Delete => self.edit(node, Edit::delete),
            Key::Left => self.edit(node, |e| {
                e.move_caret(if word { Move::WordLeft } else { Move::Left }, extend);
                None
            }),
            Key::Right => self.edit(node, |e| {
                e.move_caret(if word { Move::WordRight } else { Move::Right }, extend);
                None
            }),
            Key::Home => self.edit(node, |e| {
                e.move_caret(Move::Home, extend);
                None
            }),
            Key::End => self.edit(node, |e| {
                e.move_caret(Move::End, extend);
                None
            }),
            Key::Up | Key::Down if multiline => {
                let Some(e) = self.edits.get(&node).cloned() else { return };
                let rect =
                    self.scene.layout_rect(node).unwrap_or(Rect { x: 0.0, y: 0.0, w: 200.0, h: 100.0 });
                let size = num(&self.scene.node(node).expect("exists").props, "size", theme::size::BODY);
                let width = Some((rect.w - 2.0 * FIELD_PAD).max(1.0));
                let (cx, cy, ch) = self.layout.caret_at(e.text(), size, width, e.caret());
                let ty = if key == Key::Up { cy - ch / 2.0 } else { cy + ch * 1.5 };
                let at = self.layout.index_at(e.text(), size, width, cx, ty);
                self.edit(node, |e| {
                    e.move_caret(Move::To(at), extend);
                    None
                });
            }
            _ => {}
        }
    }

    // --- Keys --------------------------------------------------------------------

    fn key(&mut self, key: Key, mods: Mods) -> Effect {
        // Shortcuts the compositor keeps for itself.
        if key == Key::Tab && (mods.alt || mods.logo) {
            let effect =
                self.seat.handle(Input::Shortcut(Shortcut::NextWindow), &mut self.scene, &mut self.wm);
            self.restack();
            self.show_switcher(effect.raised);
            return effect;
        }
        if mods.ctrl && mods.alt {
            match key {
                Key::Left => {
                    self.switch_workspace(if self.workspace > 1 { self.workspace - 1 } else { WORKSPACES })
                }
                Key::Right => self.switch_workspace(self.workspace % WORKSPACES + 1),
                Key::Up => self.maximize_top(),
                Key::Char(c) => {
                    // Shift changes what the digit types ("!" on many layouts), so
                    // also accept the shifted symbols of a US layout.
                    let n = match c {
                        '1'..='9' => c as u32 - '0' as u32,
                        '!' => 1,
                        '@' => 2,
                        '#' => 3,
                        '$' => 4,
                        _ => 0,
                    };
                    if (1..=WORKSPACES).contains(&n) {
                        if mods.shift { self.move_top_window(n) } else { self.switch_workspace(n) }
                    }
                }
                _ => {}
            }
            return Effect::default();
        }
        if mods.logo && key == Key::Up {
            self.maximize_top();
            return Effect::default();
        }
        if self.menu.is_some() {
            self.menu_key(key);
            return Effect::default();
        }
        if key == Key::Escape {
            if let Some(window) = self.top_window()
                && let Some(d) = self.dialog_in(window)
            {
                let reply = self.apps.event(d, "cancel", None);
                self.send(reply);
            }
            return Effect::default();
        }
        if key == Key::Tab {
            self.move_focus(mods.shift);
            return Effect::default();
        }
        let Some(node) = self.focus.filter(|&n| self.scene.node(n).is_some()) else {
            return Effect::default();
        };
        let activate = matches!(key, Key::Enter | Key::Char(' '));
        match self.kind(node) {
            Some(NodeKind::TextField | NodeKind::TextArea) => self.text_key(node, key, mods),
            Some(NodeKind::Button) if activate => {
                let reply = self.apps.click(node);
                self.send(reply);
            }
            Some(NodeKind::Checkbox | NodeKind::Toggle) if activate => self.toggle(node),
            Some(NodeKind::Slider) => {
                let p = &self.scene.node(node).expect("exists").props;
                let (min, max, step) =
                    (num(p, "min", 0.0), num(p, "max", 100.0), num(p, "step", 1.0).max(0.001));
                let v = p.get("value").and_then(Value::as_f64).unwrap_or(min as f64) as f32;
                let to = match key {
                    Key::Left | Key::Down => v - step,
                    Key::Right | Key::Up => v + step,
                    Key::PageDown => v - step * 10.0,
                    Key::PageUp => v + step * 10.0,
                    Key::Home => min,
                    Key::End => max,
                    _ => return Effect::default(),
                };
                self.set_slider(node, to, min, max, step);
            }
            Some(NodeKind::Menu) if matches!(key, Key::Enter | Key::Char(' ')) => {
                if let Some(r) = self.scene.screen_rects().get(&node).map(|r| r.0) {
                    self.open_actions(node, r);
                }
            }
            Some(NodeKind::Select) => match key {
                Key::Enter | Key::Char(' ') => {
                    if let Some(r) = self.scene.screen_rects().get(&node).map(|r| r.0) {
                        self.open_select(node, r);
                    }
                }
                Key::Up | Key::Down => {
                    let options = self.options(node);
                    let current = self.prop(node, "value").and_then(Value::as_str).map(str::to_owned);
                    let at = current.and_then(|c| options.iter().position(|o| *o == c)).unwrap_or(0);
                    let next = if key == Key::Up {
                        at.saturating_sub(1)
                    } else {
                        (at + 1).min(options.len().saturating_sub(1))
                    };
                    if let Some(v) = options.get(next).cloned() {
                        self.choose_option(node, v);
                    }
                }
                _ => {}
            },
            _ => {}
        }
        Effect::default()
    }

    // --- Menus -------------------------------------------------------------------

    fn options(&self, node: NodeId) -> Vec<String> {
        self.prop(node, "options")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn choose_option(&mut self, node: NodeId, value: String) {
        if self.prop(node, "value").and_then(Value::as_str) != Some(value.as_str()) {
            self.set(node, json!({"value": value.clone()}));
            self.event(node, "change", json!(value));
        }
    }

    fn open_select(&mut self, node: NodeId, rect: Rect) {
        let options = self.options(node);
        let current = self.prop(node, "value").and_then(Value::as_str).map(str::to_owned);
        let highlight = current.and_then(|c| options.iter().position(|o| *o == c)).unwrap_or(0);
        let at = Rect { x: rect.x, y: rect.y + rect.h + 2.0, w: rect.w.max(140.0), h: 0.0 };
        self.open_menu(options.clone(), options, highlight, at, MenuFor::Select(node));
    }

    /// Open a menu widget's list of actions under it.
    fn open_actions(&mut self, node: NodeId, rect: Rect) {
        let options = self.options(node);
        let at = Rect { x: rect.x, y: rect.y + rect.h + 2.0, w: rect.w.max(140.0), h: 0.0 };
        self.open_menu(options.clone(), options, 0, at, MenuFor::Menu(node));
    }

    fn open_menu(
        &mut self,
        labels: Vec<String>,
        values: Vec<String>,
        highlight: usize,
        at: Rect,
        purpose: MenuFor,
    ) {
        self.close_menu();
        if labels.is_empty() {
            return;
        }
        let rect = Rect { h: labels.len() as f32 * ROW + 8.0, ..at };
        self.menu = Some(Menu { labels, values, highlight, rect, purpose });
        self.draw_menu();
    }

    fn draw_menu(&mut self) {
        let Some(m) = &self.menu else { return };
        let (rect, highlight) = (m.rect, m.highlight);
        let labels = m.labels.clone();
        self.scene.apply(SceneOp::Destroy { id: MENU });
        let mut ops = vec![
            create(MENU, NodeKind::Group, json!({"x": rect.x, "y": rect.y, "w": rect.w, "h": rect.h})),
            create(
                MENU + 1,
                NodeKind::Rect,
                json!({"w": rect.w, "h": rect.h, "color": theme::color::SURFACE, "radius": theme::radius::M, "border_color": theme::color::BORDER, "border_width": 1}),
            ),
            create(
                MENU + 2,
                NodeKind::Rect,
                json!({"x": 4, "y": 4.0 + highlight as f32 * ROW, "w": rect.w - 8.0, "h": ROW, "color": theme::color::SELECTION, "radius": theme::radius::S}),
            ),
            insert(MENU, MENU + 1),
            insert(MENU, MENU + 2),
            insert(ROOT, MENU),
        ];
        for (i, label) in labels.iter().enumerate() {
            let id = MENU + 10 + i as NodeId;
            ops.push(create(id, NodeKind::Text, json!({"x": 12, "y": 4.0 + i as f32 * ROW + 5.0, "w": rect.w - 24.0, "h": 18, "text": label, "size": 14, "color": theme::color::TEXT})));
            ops.push(insert(MENU, id));
        }
        self.scene.apply_all(ops);
    }

    fn close_menu(&mut self) {
        if self.menu.take().is_some() {
            self.scene.apply(SceneOp::Destroy { id: MENU });
        }
    }

    fn menu_press(&mut self, x: f32, y: f32) {
        let Some(m) = &self.menu else { return };
        if !m.rect.contains(x, y) {
            self.close_menu();
            return;
        }
        let i = ((y - m.rect.y - 4.0) / ROW).floor().max(0.0) as usize;
        if i < m.values.len() {
            self.menu.as_mut().expect("open").highlight = i;
            self.choose_menu();
        }
    }

    fn menu_key(&mut self, key: Key) {
        let Some(m) = &mut self.menu else { return };
        match key {
            Key::Up => m.highlight = m.highlight.saturating_sub(1),
            Key::Down => m.highlight = (m.highlight + 1).min(m.values.len() - 1),
            Key::Enter | Key::Char(' ') => return self.choose_menu(),
            Key::Escape => return self.close_menu(),
            _ => return,
        }
        self.draw_menu();
    }

    fn choose_menu(&mut self) {
        let Some(m) = self.menu.take() else { return };
        self.scene.apply(SceneOp::Destroy { id: MENU });
        let Some(value) = m.values.get(m.highlight).cloned() else { return };
        match m.purpose {
            MenuFor::Select(node) => self.choose_option(node, value),
            MenuFor::Menu(node) => self.event(node, "choose", json!(value)),
            MenuFor::Launcher => self.request(None, "session", "launch", json!({"app": value})),
        }
    }

    // --- Workspaces and windows ----------------------------------------------------

    /// Show workspace `n`: its windows appear and the others' hide.
    pub fn switch_workspace(&mut self, n: u32) {
        self.workspace = n.clamp(1, WORKSPACES);
        self.apps.workspace = self.workspace;
        let frames: Vec<NodeId> = self
            .scene
            .top_level()
            .iter()
            .copied()
            .filter(|&id| self.prop(id, "window").and_then(Value::as_bool) == Some(true))
            .collect();
        for id in frames {
            let ws = self.prop(id, "workspace").and_then(Value::as_u64).unwrap_or(1) as u32;
            let hidden = ws != self.workspace;
            if self.prop(id, "hidden").and_then(Value::as_bool).unwrap_or(false) != hidden {
                self.set(id, json!({"hidden": hidden}));
            }
        }
        if self.focus.is_some_and(|f| self.top_window().is_none_or(|w| !self.scene.is_within(f, w))) {
            self.set_focus(None);
        }
        shell::set_workspace(&mut self.scene, self.workspace);
        log::info!("wm: workspace {}", self.workspace);
    }

    /// Send the topmost window to workspace `n`.
    fn move_top_window(&mut self, n: u32) {
        let Some(window) = self.top_window() else { return };
        self.set(window, json!({"workspace": n}));
        self.switch_workspace(self.workspace);
    }

    fn maximize_top(&mut self) {
        if let Some(window) = self.top_window().and_then(|f| self.apps.window_of(f)) {
            let reply = self.apps.toggle_maximize(&mut self.scene, window);
            self.send(reply);
        }
    }

    /// Show the window switcher for a moment, with `raised` highlighted.
    fn show_switcher(&mut self, raised: Option<NodeId>) {
        let windows: Vec<(NodeId, String)> = self
            .scene
            .top_level()
            .iter()
            .rev()
            .copied()
            .filter(|&id| wm::is_window(&self.scene, id))
            .map(|id| (id, self.prop(id, "title").and_then(Value::as_str).unwrap_or("Window").to_owned()))
            .take(8)
            .collect();
        self.scene.apply(SceneOp::Destroy { id: SWITCHER });
        if windows.is_empty() {
            return;
        }
        let (sw, sh) = self.seat.bounds();
        let (w, h) = (320.0, windows.len() as f32 * ROW + 16.0);
        let mut ops = vec![
            create(
                SWITCHER,
                NodeKind::Group,
                json!({"x": (sw - w) / 2.0, "y": (sh - h) / 3.0, "w": w, "h": h}),
            ),
            create(
                SWITCHER + 1,
                NodeKind::Rect,
                json!({"w": w, "h": h, "color": "#ffffffee", "radius": theme::radius::L, "border_color": theme::color::BORDER, "border_width": 1}),
            ),
            insert(SWITCHER, SWITCHER + 1),
            insert(ROOT, SWITCHER),
        ];
        for (i, (id, title)) in windows.iter().enumerate() {
            let y = 8.0 + i as f32 * ROW;
            if Some(*id) == raised {
                ops.push(create(SWITCHER + 2, NodeKind::Rect, json!({"x": 8, "y": y, "w": w - 16.0, "h": ROW, "color": theme::color::SELECTION, "radius": theme::radius::S})));
                ops.push(insert(SWITCHER, SWITCHER + 2));
            }
            let t = SWITCHER + 10 + i as NodeId;
            ops.push(create(t, NodeKind::Text, json!({"x": 18, "y": y + 5.0, "w": w - 36.0, "h": 18, "text": title, "size": 14, "color": theme::color::TEXT})));
            ops.push(insert(SWITCHER, t));
        }
        self.scene.apply_all(ops);
        self.switcher_until = Some(Instant::now() + SWITCHER_SHOWS);
    }

    // --- Time ----------------------------------------------------------------------

    /// Move animations along to `now`: coasting scroll views, the switcher's
    /// timeout, the clock. Returns whether something is still animating.
    pub fn tick(&mut self, now: Instant) -> bool {
        let dt = self.last_tick.map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f32()).min(0.1);
        self.last_tick = Some(now);
        let mut still = Vec::new();
        for (node, v) in std::mem::take(&mut self.coasting) {
            if self.scene.node(node).is_none() {
                continue;
            }
            let before = self.prop(node, "_scroll").and_then(Value::as_f64).unwrap_or(0.0);
            self.scroll_by(node, v * dt);
            let after = self.prop(node, "_scroll").and_then(Value::as_f64).unwrap_or(0.0);
            // Friction; and stop at either end.
            let v = v * (-4.0 * dt).exp();
            if v.abs() > 20.0 && (dt == 0.0 || before != after) {
                still.push((node, v));
            }
        }
        self.coasting = still;
        if self.switcher_until.is_some_and(|t| now >= t) {
            self.switcher_until = None;
            self.scene.apply(SceneOp::Destroy { id: SWITCHER });
        }
        if self.scene.node(TOP_BAR).is_some() {
            shell::set_clock(&mut self.scene, &shell::clock_text(SystemTime::now()));
        }
        !self.coasting.is_empty()
    }

    /// When [`Desktop::tick`] next has something to do.
    pub fn next_wake(&self, now: Instant) -> Instant {
        if !self.coasting.is_empty() {
            return now + Duration::from_millis(16);
        }
        let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let minute = now + Duration::from_secs(60 - secs % 60);
        self.switcher_until.map_or(minute, |t| t.min(minute))
    }

    // --- The bus ---------------------------------------------------------------------

    /// Handle one request, cancel, or answer from the bus.
    pub fn call(&mut self, call: Call) -> Vec<Out> {
        self.relayout();
        match call {
            Call::Cancel { id } if self.prompts.iter().any(|(p, _)| *p == id) => {
                // permd gave up waiting (it timed out).
                let first = self.prompts.front().is_some_and(|(p, _)| *p == id);
                self.prompts.retain(|(p, _)| *p != id);
                if first {
                    self.show_prompt();
                }
            }
            Call::Grants(table) => self.take_grants(table),
            Call::Cancel { id } => {
                if self.apps.close_request(&mut self.scene, id) {
                    log::info!("apps: closed a window (request {id} ended)");
                    self.forget_gone();
                }
            }
            Call::Reply { token, result } => self.reply(token, result),
            Call::Blob { hash, app, result } => self.blob_loaded(hash, app, result),
            Call::Request { id, from, method, params, stream } => {
                self.request_in(id, from, &method, params, stream)
            }
        }
        self.drain()
    }

    /// Drop state for nodes that no longer exist.
    fn forget_gone(&mut self) {
        let scene = &self.scene;
        self.edits.retain(|n, _| scene.node(*n).is_some());
        if self.focus.is_some_and(|f| scene.node(f).is_none()) {
            self.focus = None;
        }
        self.coasting.retain(|(n, _)| scene.node(*n).is_some());
        if let Some(Drag::Slider(n) | Drag::Text(n) | Drag::Split(n) | Drag::Scroll { node: n, .. }) =
            &self.drag
            && scene.node(*n).is_none()
        {
            self.drag = None;
        }
    }

    fn request_in(&mut self, id: u64, from: Option<Sender>, method: &str, params: Value, stream: bool) {
        let fail = |message: String| Out::Respond { id, result: Err(bad_request(message)) };
        // The broker stamps every request; a window belongs to whoever opened it.
        let Some(from) = from else { return self.outbox.push(fail("requests need a sender".into())) };
        match method {
            method::OPEN_WINDOW if !stream => {
                self.outbox.push(fail("open_window must be sent as a streamed request".into()))
            }
            method::OPEN_WINDOW => match parse::<OpenWindowParams>(params) {
                Ok(p) => {
                    let screen = self.seat.bounds();
                    // One line per window; tools/smoke-checks.ts counts them.
                    log::info!("apps: opened a window for {from:?}: {:?}", p.title);
                    let reply = self.apps.open(&mut self.scene, from, id, &p, screen);
                    self.restack();
                    self.send(Some(reply));
                }
                Err(e) => self.outbox.push(fail(e)),
            },
            method::COMMIT => {
                let result =
                    parse::<Commit>(params).and_then(|c| self.apps.commit(&mut self.scene, &from, c));
                match result {
                    Ok(created) => {
                        self.forget_gone();
                        self.load_bound(&created);
                        self.catch_up_focus();
                        self.outbox.push(Out::Respond { id, result: Ok(Value::Null) });
                    }
                    Err(e) => self.outbox.push(fail(e)),
                }
            }
            method::GET_CLIPBOARD => {
                if let Sender::App { app, .. } = &from
                    && !self.grants.get(app).is_some_and(|g| g.has(capability::CLIPBOARD))
                {
                    let message = format!("{app} may not read the clipboard");
                    let e = BusError { code: protocol::bus::code::PERMISSION_DENIED.into(), message };
                    return self.outbox.push(Out::Respond { id, result: Err(e) });
                }
                let value = serde_json::to_value(&self.clipboard).expect("serializes");
                self.outbox.push(Out::Respond { id, result: Ok(value) });
            }
            method::PROMPT => {
                if from != Sender::Service(protocol::perm::SERVICE.into()) {
                    let message = "only perm may prompt".to_owned();
                    let e = BusError { code: protocol::bus::code::PERMISSION_DENIED.into(), message };
                    return self.outbox.push(Out::Respond { id, result: Err(e) });
                }
                match parse::<PromptParams>(params) {
                    Ok(p) => {
                        self.prompts.push_back((id, p));
                        if self.prompts.len() == 1 {
                            self.show_prompt();
                        }
                    }
                    Err(e) => self.outbox.push(fail(e)),
                }
            }
            method::SET_CLIPBOARD => match parse::<Clipboard>(params) {
                Ok(c) => {
                    self.clipboard = c;
                    self.outbox.push(Out::Respond { id, result: Ok(Value::Null) });
                }
                Err(e) => self.outbox.push(fail(e)),
            },
            other => self.outbox.push(fail(format!("the compositor has no method {other:?}"))),
        }
    }

    /// If the app just replaced the focused text widget's text, move the caret
    /// to fit it.
    fn catch_up_focus(&mut self) {
        let Some(node) = self.focus else { return };
        if !matches!(self.kind(node), Some(NodeKind::TextField | NodeKind::TextArea)) {
            return;
        }
        let shown = self.text_value(node);
        if self.edits.get(&node).is_some_and(|e| e.text() != shown) {
            self.edit(node, |_| None);
        }
    }

    /// Fetch the documents behind any newly created bound text fields, and the
    /// blobs behind new images.
    fn load_bound(&mut self, created: &[NodeId]) {
        for &node in created {
            if self.kind(node) == Some(NodeKind::Image)
                && let Some(hash) = self.prop(node, "blob").and_then(Value::as_str).map(str::to_owned)
            {
                // Only well-formed hashes go to dbd (and into log lines).
                if !is_blob_hash(&hash) {
                    log::warn!(
                        "an image names {:?}, which isn't a blob hash",
                        hash.chars().take(16).collect::<String>()
                    );
                    continue;
                }
                let who = self.apps.app_of(node).map(str::to_owned);
                let key = (who.clone(), hash.clone());
                self.uses += 1;
                let cached = self.images.get_mut(&hash).and_then(|c| {
                    c.used = self.uses;
                    c.image.clone()
                });
                match cached {
                    Some(img) if self.blob_ok.contains(&key) => {
                        self.set(node, json!({"_iw": img.width, "_ih": img.height}));
                    }
                    // Not yet checked for this app, or not loaded yet: fetch as the app.
                    _ if self.fetching.insert(key) => {
                        self.outbox.push(Out::FetchBlob { hash, app: who });
                    }
                    _ => {}
                }
                continue;
            }
            if !matches!(self.kind(node), Some(NodeKind::TextField | NodeKind::TextArea)) {
                continue;
            }
            let Some(doc) = self.prop(node, "bind").and_then(|b| b.get("doc")).and_then(Value::as_str) else {
                continue;
            };
            let mut params = json!({"id": doc});
            self.act_for(node, &mut params);
            self.request(Some(Pending::Bind(node)), "db", "get", params);
        }
    }

    /// If an app owns `node`, ask `db` to treat the request as that app's: the
    /// compositor reads and writes documents for an app only as the app may.
    fn act_for(&self, node: NodeId, params: &mut Value) {
        if let (Some(app), Some(p)) = (self.apps.app_of(node), params.as_object_mut()) {
            p.insert("as".into(), json!(app));
        }
    }

    /// A new grant table: keep it if it's newer, and forget which blobs the apps
    /// whose grants changed were allowed (their images check again).
    fn take_grants(&mut self, table: GrantsResult) {
        if table.version < self.grants_version {
            return;
        }
        self.grants_version = table.version;
        let new = protocol::perm::by_app(&table.grants);
        let old = std::mem::replace(&mut self.grants, new);
        let changed = |app: &String| old.get(app) != self.grants.get(app);
        self.blob_ok.retain(|(app, _)| !app.as_ref().is_some_and(changed));
        // Images of those apps go blank until checked again.
        let recheck: Vec<NodeId> = self
            .scene
            .ids()
            .filter(|&id| {
                self.kind(id) == Some(NodeKind::Image)
                    && self.apps.app_of(id).is_some_and(|a| old.get(a) != self.grants.get(a))
            })
            .collect();
        for &id in &recheck {
            self.set(id, json!({"_iw": 0, "_ih": 0}));
        }
        self.load_bound(&recheck);
    }

    // --- Permission prompts ------------------------------------------------------

    /// Show the first waiting prompt, or take the layer down if none is.
    fn show_prompt(&mut self) {
        self.scene.apply(SceneOp::Destroy { id: prompt::PROMPT });
        self.shown_prompt = None;
        let Some((_, p)) = self.prompts.front() else { return };
        let (w, h) = self.seat.bounds();
        let (ops, shown) = prompt::ops(p, w, h, self.allow_focused);
        self.close_menu();
        self.drag = None;
        self.wm.pointer_up();
        self.scene.apply_all(ops);
        self.shown_prompt = Some(shown);
        self.restack();
    }

    /// Answer the shown prompt and move on to the next.
    fn answer_prompt(&mut self, choice: prompt::Choice) {
        let Some((id, p)) = self.prompts.pop_front() else { return };
        let allow = choice == prompt::Choice::Allow;
        log::info!("prompt: the user {} {}", if allow { "allowed" } else { "turned down" }, p.app);
        self.outbox.push(Out::Respond { id, result: Ok(json!({"allow": allow})) });
        self.allow_focused = false;
        self.show_prompt();
    }

    /// Input while a prompt is up: it goes to the prompt alone.
    fn prompt_input(&mut self, input: Input) -> Effect {
        match input {
            // The pointer moves (no window drag is going on), but nothing under the
            // prompt hears it.
            Input::Motion { .. } | Input::MotionTo { .. } => {
                self.seat.handle(input, &mut self.scene, &mut self.wm)
            }
            Input::Button { pressed: true } => {
                let (x, y) = self.seat.cursor();
                if let Some(choice) = self.shown_prompt.and_then(|s| s.hit(x, y)) {
                    self.answer_prompt(choice);
                }
                Effect::default()
            }
            Input::Key { key, .. } => {
                match key {
                    Key::Escape => self.answer_prompt(prompt::Choice::Deny),
                    Key::Tab | Key::Left | Key::Right => {
                        self.allow_focused = !self.allow_focused;
                        self.show_prompt();
                    }
                    Key::Enter | Key::Char(' ') => {
                        let choice =
                            if self.allow_focused { prompt::Choice::Allow } else { prompt::Choice::Deny };
                        self.answer_prompt(choice);
                    }
                    _ => {}
                }
                Effect::default()
            }
            Input::Button { pressed: false } | Input::Scroll { .. } | Input::Shortcut(_) => Effect::default(),
        }
    }

    /// Blob `hash` arrived for `app`: decode it (unless it's cached already)
    /// and size every image of that app showing it.
    fn blob_loaded(&mut self, hash: String, app: Option<String>, result: Result<Vec<u8>, String>) {
        self.fetching.remove(&(app.clone(), hash.clone()));
        // A failed fetch (db refused this app) never falls back to pixels cached
        // for someone else; a good one reuses them rather than decoding again.
        let img = result.and_then(|bytes| match self.images.get(&hash).and_then(|c| c.image.clone()) {
            Some(img) => Ok(img),
            None => crate::image::decode_png(&bytes),
        });
        let img = match img {
            Ok(img) => img,
            Err(e) => {
                log::warn!("image {}: {e}", hash.get(..12).unwrap_or(&hash));
                return;
            }
        };
        let (w, h) = (img.width, img.height);
        self.uses += 1;
        self.images.insert(hash.clone(), Cached { image: Some(img), used: self.uses });
        self.blob_ok.insert((app.clone(), hash.clone()));
        let nodes: Vec<NodeId> = self
            .scene
            .ids()
            .filter(|&id| {
                self.kind(id) == Some(NodeKind::Image)
                    && self.prop(id, "blob").and_then(Value::as_str) == Some(hash.as_str())
                    && self.apps.app_of(id) == app.as_deref()
            })
            .collect();
        for id in nodes {
            self.set(id, json!({"_iw": w, "_ih": h}));
        }
        self.evict_images();
    }

    /// Drop the least recently wanted images no node shows, while those take
    /// more than [`IMAGE_CACHE_BYTES`].
    fn evict_images(&mut self) {
        let shown: std::collections::HashSet<String> = self
            .scene
            .ids()
            .filter(|&id| self.kind(id) == Some(NodeKind::Image))
            .filter_map(|id| self.prop(id, "blob").and_then(Value::as_str).map(str::to_owned))
            .collect();
        let size = |c: &Cached| c.image.as_ref().map_or(0, |i| i.rgba.len());
        let mut idle: Vec<(u64, String, usize)> = self
            .images
            .iter()
            .filter(|(h, _)| !shown.contains(*h))
            .map(|(h, c)| (c.used, h.clone(), size(c)))
            .collect();
        let mut total: usize = idle.iter().map(|i| i.2).sum();
        idle.sort();
        for (_, hash, bytes) in idle {
            if total <= IMAGE_CACHE_BYTES {
                break;
            }
            self.images.remove(&hash);
            self.blob_ok.retain(|(_, h)| *h != hash);
            total -= bytes;
        }
    }

    fn reply(&mut self, token: u64, result: Result<Value, BusError>) {
        let Some(pending) = self.pending.remove(&token) else {
            if let Err(e) = result {
                log::warn!("a compositor request failed: {}: {}", e.code, e.message);
            }
            return;
        };
        match (pending, result) {
            (Pending::Bind(node), Ok(doc)) => {
                if self.scene.node(node).is_none() {
                    return;
                }
                let path = self
                    .prop(node, "bind")
                    .and_then(|b| b.get("path"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let mut at = doc.get("data");
                for key in path.split('.') {
                    at = at.and_then(|v| v.get(key));
                }
                let text = at.and_then(Value::as_str).unwrap_or_default().to_owned();
                self.set(node, json!({"_value": text.clone()}));
                if let Some(e) = self.edits.get_mut(&node) {
                    e.set_text(&text);
                }
                if self.focus == Some(node) {
                    self.show_caret(node);
                }
            }
            (Pending::Launcher, Ok(found)) => {
                let docs = found.get("docs").and_then(Value::as_array).cloned().unwrap_or_default();
                let mut apps: Vec<(String, String)> = docs
                    .iter()
                    .filter_map(|d| {
                        let data = d.get("data")?;
                        Some((data.get("name")?.as_str()?.to_owned(), data.get("id")?.as_str()?.to_owned()))
                    })
                    .collect();
                apps.sort();
                let at = shell::launcher_rect(&self.scene);
                let rect = Rect { x: at.x, y: at.y + at.h + 4.0, w: 200.0, h: 0.0 };
                let (labels, values) = apps.into_iter().unzip();
                self.open_menu(labels, values, 0, rect, MenuFor::Launcher);
            }
            (_, Err(e)) => log::warn!("a compositor request failed: {}: {}", e.code, e.message),
        }
    }
}

fn part(reply: Reply) -> Out {
    Out::Part { id: reply.request, data: serde_json::to_value(reply.event).expect("events serialize") }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, String> {
    serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))
}

fn bad_request(message: String) -> BusError {
    BusError { code: code::BAD_REQUEST.into(), message }
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
    use crate::demo;
    use protocol::WindowEvent;

    fn app() -> Option<Sender> {
        Some(Sender::App { app: "org.example.test".into(), conn: 3 })
    }

    /// A desktop with the shell and no demo windows, measuring with the bundled font.
    fn desk() -> Desktop {
        let mut d = Desktop::with_fonts(1280.0, 800.0, crate::fonts::ui_only());
        d.scene.apply_all(demo::build(1280.0, 800.0, 0));
        d.set_screen(1280.0, 800.0);
        d
    }

    fn request(id: u64, method: &str, params: Value, stream: bool) -> Call {
        Call::Request { id, from: app(), method: method.into(), params, stream }
    }

    /// Open a window for request `request` and commit `ops` to it. Returns its id.
    fn open(d: &mut Desktop, request_id: u64, ops: Value) -> u64 {
        let out = d.call(request(
            request_id,
            "open_window",
            json!({"title": "Test", "width": 400, "height": 300}),
            true,
        ));
        let [Out::Part { data, .. }] = &out[..] else { panic!("{out:?}") };
        let WindowEvent::Opened { window, .. } = serde_json::from_value(data.clone()).unwrap() else {
            panic!()
        };
        let out = d.call(request(request_id + 1000, "commit", json!({"window": window, "ops": ops}), false));
        assert!(matches!(out.last(), Some(Out::Respond { result: Ok(_), .. })), "{out:?}");
        window
    }

    /// The app's id for scene node `global` (tests name nodes by app id).
    fn global(d: &Desktop, window: u64, app_id: NodeId) -> NodeId {
        let frame = d.apps.frame_of(window).unwrap();
        d.scene
            .paint_order(frame)
            .into_iter()
            .map(|(id, ..)| id)
            .find(|&id| {
                d.apps
                    .event(id, "x", None)
                    .is_some_and(|r| matches!(r.event, WindowEvent::Event { node, .. } if node == app_id))
            })
            .unwrap_or_else(|| panic!("no node {app_id}"))
    }

    fn center(d: &mut Desktop, node: NodeId) -> (f32, f32) {
        d.relayout();
        let (r, _) = d.scene.screen_rects()[&node];
        (r.x + r.w / 2.0, r.y + r.h / 2.0)
    }

    fn click(d: &mut Desktop, (x, y): (f32, f32)) -> Vec<Out> {
        let mut out = d.input(Input::MotionTo { x, y }).1;
        out.extend(d.input(Input::Button { pressed: true }).1);
        out.extend(d.input(Input::Button { pressed: false }).1);
        out
    }

    fn key(d: &mut Desktop, key: Key) -> Vec<Out> {
        d.input(Input::Key { key, mods: Mods::default() }).1
    }

    fn typed(d: &mut Desktop, s: &str) -> Vec<Out> {
        s.chars().flat_map(|c| key(d, Key::Char(c))).collect()
    }

    /// The (node, event, value) of every window event in `out`.
    fn events(out: &[Out]) -> Vec<(NodeId, String, Value)> {
        out.iter()
            .filter_map(|o| match o {
                Out::Part { data, .. } => match serde_json::from_value(data.clone()).ok()? {
                    WindowEvent::Event { node, event, value } => {
                        Some((node, event, value.unwrap_or(Value::Null)))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    fn prop(d: &Desktop, node: NodeId, key: &str) -> Value {
        d.scene.node(node).unwrap().props.get(key).cloned().unwrap_or(Value::Null)
    }

    fn widgets() -> Value {
        json!([
            {"op": "create", "id": 2, "type": "vstack", "props": {"padding": 12, "gap": 10}},
            {"op": "create", "id": 3, "type": "checkbox", "props": {"label": "Wi-Fi"}},
            {"op": "create", "id": 4, "type": "toggle", "props": {"label": "Dark", "checked": true}},
            {"op": "create", "id": 5, "type": "slider", "props": {"min": 0, "max": 10, "step": 1, "value": 2, "w": 200}},
            {"op": "create", "id": 6, "type": "select", "props": {"options": ["A", "B", "C"], "value": "A"}},
            {"op": "create", "id": 7, "type": "text_field", "props": {"placeholder": "Name"}},
            {"op": "create", "id": 8, "type": "button", "props": {"label": "Go"}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0},
            {"op": "insert", "parent": 2, "child": 3, "index": 0},
            {"op": "insert", "parent": 2, "child": 4, "index": 1},
            {"op": "insert", "parent": 2, "child": 5, "index": 2},
            {"op": "insert", "parent": 2, "child": 6, "index": 3},
            {"op": "insert", "parent": 2, "child": 7, "index": 4},
            {"op": "insert", "parent": 2, "child": 8, "index": 5},
            {"op": "listen", "id": 8, "events": ["click"]}
        ])
    }

    #[test]
    fn checkboxes_toggles_sliders_and_selects_change_and_tell_the_app() {
        let mut d = desk();
        let w = open(&mut d, 7, widgets());
        let (cb, tg, sl, se) = (global(&d, w, 3), global(&d, w, 4), global(&d, w, 5), global(&d, w, 6));

        let at = center(&mut d, cb);
        assert_eq!(events(&click(&mut d, at)), [(3, "change".into(), json!(true))]);
        assert_eq!(prop(&d, cb, "checked"), true);
        let at = center(&mut d, tg);
        assert_eq!(events(&click(&mut d, at)), [(4, "change".into(), json!(false))]);

        // A press three quarters along the slider's track.
        d.relayout();
        let (r, _) = d.scene.screen_rects()[&sl];
        let x = r.x + 8.0 + (r.w - 16.0) * 0.75;
        let out = click(&mut d, (x, r.y + r.h / 2.0));
        assert_eq!(events(&out).last().unwrap().2, json!(8.0), "0..10 in steps of 1, rounded");
        assert_eq!(events(&key(&mut d, Key::Right)), [(5, "change".into(), json!(9.0))]);
        key(&mut d, Key::Right);
        assert!(events(&key(&mut d, Key::Right)).is_empty(), "already at the maximum");

        // The select opens a menu; choosing from it changes the value.
        let at = center(&mut d, se);
        click(&mut d, at);
        assert!(d.scene.node(MENU).is_some());
        let list = d.draw_list();
        let c = list.layers.iter().flat_map(|l| &l.texts).find(|t| t.node > MENU && t.text == "C").unwrap();
        let out = click(&mut d, (c.rect.x + 4.0, c.rect.y + 4.0));
        assert_eq!(events(&out), [(6, "change".into(), json!("C"))]);
        assert!(d.scene.node(MENU).is_none(), "closed");
        // And from the keyboard: focus is on the select now.
        assert_eq!(events(&key(&mut d, Key::Up)), [(6, "change".into(), json!("B"))]);
    }

    #[test]
    fn menus_offer_actions_and_report_the_one_chosen() {
        let mut d = desk();
        let w = open(
            &mut d,
            7,
            json!([
                {"op": "create", "id": 2, "type": "vstack", "props": {"padding": 12}},
                {"op": "create", "id": 3, "type": "menu", "props": {"label": "File", "options": ["New", "Open", "Close"]}},
                {"op": "insert", "parent": 1, "child": 2, "index": 0},
                {"op": "insert", "parent": 2, "child": 3, "index": 0}
            ]),
        );
        let m = global(&d, w, 3);
        let at = center(&mut d, m);
        assert!(events(&click(&mut d, at)).is_empty(), "opening says nothing yet");
        let list = d.draw_list();
        let item =
            list.layers.iter().flat_map(|l| &l.texts).find(|t| t.node > MENU && t.text == "Open").unwrap();
        let out = click(&mut d, (item.rect.x + 4.0, item.rect.y + 4.0));
        assert_eq!(events(&out), [(3, "choose".into(), json!("Open"))]);
        assert!(d.scene.node(MENU).is_none(), "closed");
        assert_eq!(prop(&d, m, "label"), "File", "a menu keeps its label");
        assert!(prop(&d, m, "value").is_null(), "and has no value");
        // From the keyboard: Enter opens it, Down moves, Enter chooses.
        key(&mut d, Key::Enter);
        assert!(d.scene.node(MENU).is_some());
        key(&mut d, Key::Down);
        key(&mut d, Key::Down);
        assert_eq!(events(&key(&mut d, Key::Enter)), [(3, "choose".into(), json!("Close"))]);
    }

    #[test]
    fn text_fields_edit_locally_and_report_changes_and_submit() {
        let mut d = desk();
        let w = open(&mut d, 7, widgets());
        let field = global(&d, w, 7);
        let at = center(&mut d, field);
        click(&mut d, at);
        assert_eq!(prop(&d, field, "_focus"), true);
        let out = typed(&mut d, "hello");
        assert_eq!(events(&out).last().unwrap(), &(7, "change".into(), json!("hello")));
        key(&mut d, Key::Backspace);
        assert_eq!(prop(&d, field, "_value"), "hell");
        assert!(prop(&d, field, "_caret").as_array().unwrap()[0].as_f64().unwrap() > 0.0);
        // Select all, copy, then paste twice at the end.
        let primary = Mods { logo: true, ..Mods::default() };
        d.input(Input::Key { key: Key::Char('a'), mods: primary });
        assert_eq!(prop(&d, field, "_sel").as_array().unwrap().len(), 1, "one highlighted run");
        d.input(Input::Key { key: Key::Char('c'), mods: primary });
        assert_eq!(d.clipboard.text, "hell");
        key(&mut d, Key::Right);
        d.input(Input::Key { key: Key::Char('v'), mods: primary });
        assert_eq!(prop(&d, field, "_value"), "hellhell");
        assert_eq!(events(&key(&mut d, Key::Enter)), [(7, "submit".into(), json!("hellhell"))]);
        // Clicking elsewhere takes focus away.
        click(&mut d, (1200.0, 700.0));
        assert_eq!(prop(&d, field, "_focus"), false);
    }

    #[test]
    fn an_app_can_replace_what_was_typed() {
        let mut d = desk();
        let w = open(&mut d, 7, widgets());
        let field = global(&d, w, 7);
        let at = center(&mut d, field);
        click(&mut d, at);
        typed(&mut d, "draft");
        let clear = json!({"window": w, "ops": [{"op": "set", "id": 7, "props": {"value": ""}}]});
        d.call(request(9, "commit", clear, false));
        assert_eq!(prop(&d, field, "_value"), "");
        assert_eq!(prop(&d, field, "_caret")[0], 0.0, "the caret moved back to the start");
        let out = typed(&mut d, "x");
        assert_eq!(events(&out), [(7, "change".into(), json!("x"))], "editing starts from the app's value");
    }

    #[test]
    fn late_echoes_of_typed_text_dont_undo_typing() {
        // An app that sets the field's value from each change event, as the
        // gallery does. Its answers arrive after the typing.
        let mut d = desk();
        let w = open(&mut d, 7, widgets());
        let field = global(&d, w, 7);
        let at = center(&mut d, field);
        click(&mut d, at);
        typed(&mut d, "Grace");
        let set = |d: &mut Desktop, v: &str| {
            let ops = json!({"window": w, "ops": [{"op": "set", "id": 7, "props": {"value": v}}]});
            d.call(request(9, "commit", ops, false));
        };
        set(&mut d, "G");
        assert_eq!(prop(&d, field, "_value"), "Grace", "an old echo leaves the typing alone");
        typed(&mut d, "!");
        for v in ["Gr", "Gra", "Grac", "Grace"] {
            set(&mut d, v);
        }
        assert_eq!(prop(&d, field, "_value"), "Grace!");
        assert_eq!(
            events(&typed(&mut d, "?")),
            [(7, "change".into(), json!("Grace!?"))],
            "the caret stayed at the end"
        );
        // A value the field never reported is the app's own, and replaces the text.
        set(&mut d, "Ada");
        assert_eq!(prop(&d, field, "_value"), "Ada");
        // So is clearing it, even after an echo.
        typed(&mut d, "x");
        set(&mut d, "Adax");
        set(&mut d, "");
        assert_eq!(prop(&d, field, "_value"), "");
    }

    #[test]
    fn tab_moves_focus_and_the_keyboard_works_widgets() {
        let mut d = desk();
        let w = open(&mut d, 7, widgets());
        let order: Vec<NodeId> = [3, 4, 5, 6, 7, 8].iter().map(|&n| global(&d, w, n)).collect();
        let mut seen = Vec::new();
        for _ in 0..7 {
            key(&mut d, Key::Tab);
            seen.push(d.focus.unwrap());
        }
        assert_eq!(&seen[..6], &order[..], "in order");
        assert_eq!(seen[6], order[0], "and around again");
        assert_eq!(events(&key(&mut d, Key::Char(' '))), [(3, "change".into(), json!(true))]);
        d.input(Input::Key { key: Key::Tab, mods: Mods { shift: true, ..Mods::default() } });
        assert_eq!(d.focus, Some(order[5]), "Shift+Tab goes back");
        assert_eq!(events(&key(&mut d, Key::Enter)), [(8, "click".into(), Value::Null)]);
        assert_eq!(prop(&d, order[5], "_focus"), true);
    }

    #[test]
    fn scroll_views_scroll_with_the_wheel_and_coast_after_a_drag() {
        let mut d = desk();
        let mut ops = vec![
            json!({"op": "create", "id": 2, "type": "scroll", "props": {"padding": 4}}),
            json!({"op": "insert", "parent": 1, "child": 2, "index": 0}),
        ];
        for i in 0..40u64 {
            ops.push(
                json!({"op": "create", "id": 10 + i, "type": "label", "props": {"text": format!("Row {i}")}}),
            );
            ops.push(json!({"op": "insert", "parent": 2, "child": 10 + i, "index": i}));
        }
        let w = open(&mut d, 7, Value::Array(ops));
        let scroll = d.scene.parent(global(&d, w, 10)).unwrap();
        d.relayout();
        let content = prop(&d, scroll, "_content").as_f64().unwrap();
        assert!(content > 300.0, "40 rows: {content}");
        let at = center(&mut d, scroll);
        d.input(Input::MotionTo { x: at.0, y: at.1 });
        d.input(Input::Scroll { dx: 0.0, dy: 100.0 });
        assert_eq!(prop(&d, scroll, "_scroll"), 100.0);
        d.input(Input::Scroll { dx: 0.0, dy: -500.0 });
        assert_eq!(prop(&d, scroll, "_scroll"), 0.0, "clamped at the top");

        // Drag up quickly and let go: it keeps moving, slowing, then stops.
        d.input(Input::Button { pressed: true });
        for step in 1..=4 {
            d.input(Input::MotionTo { x: at.0, y: at.1 - 10.0 * step as f32 });
        }
        d.input(Input::Button { pressed: false });
        let after_drag = prop(&d, scroll, "_scroll").as_f64().unwrap();
        assert_eq!(after_drag, 40.0);
        let t0 = Instant::now();
        d.tick(t0);
        let mut t = t0;
        let mut moving = true;
        for _ in 0..200 {
            t += Duration::from_millis(16);
            moving = d.tick(t);
            if !moving {
                break;
            }
        }
        assert!(!moving, "it comes to rest");
        assert!(prop(&d, scroll, "_scroll").as_f64().unwrap() > after_drag + 20.0, "it coasted on");
    }

    #[test]
    fn virtual_lists_report_their_visible_rows() {
        let mut d = desk();
        let ops = json!([
            {"op": "create", "id": 3, "type": "vstack", "props": {}},
            {"op": "create", "id": 2, "type": "vlist", "props": {"count": 100, "row_height": 20, "h": 100}},
            {"op": "insert", "parent": 1, "child": 3, "index": 0},
            {"op": "insert", "parent": 3, "child": 2, "index": 0}
        ]);
        let out = d.call(request(7, "open_window", json!({"title": "List"}), true));
        let window = match &out[..] {
            [Out::Part { data, .. }] => data["window"].as_u64().unwrap(),
            other => panic!("{other:?}"),
        };
        d.call(request(8, "commit", json!({"window": window, "ops": ops}), false));
        let out = d.call(Call::Cancel { id: 999 }); // any call relays out
        let first = events(&out);
        assert_eq!(first, [(2, "range".into(), json!([0, 4]))]);
        let vlist = global(&d, window, 2);
        let at = center(&mut d, vlist);
        d.input(Input::MotionTo { x: at.0, y: at.1 });
        let out = d.input(Input::Scroll { dx: 0.0, dy: 210.0 }).1;
        assert_eq!(events(&out), [(2, "range".into(), json!([10, 15]))]);
    }

    #[test]
    fn dialogs_block_the_rest_of_their_window_and_escape_cancels() {
        let mut d = desk();
        let w = open(
            &mut d,
            7,
            json!([
                {"op": "create", "id": 2, "type": "vstack", "props": {"padding": 12}},
                {"op": "create", "id": 3, "type": "button", "props": {"label": "Behind"}},
                {"op": "create", "id": 4, "type": "dialog", "props": {"w": 200}},
                {"op": "create", "id": 5, "type": "button", "props": {"label": "OK"}},
                {"op": "insert", "parent": 1, "child": 2, "index": 0},
                {"op": "insert", "parent": 2, "child": 3, "index": 0},
                {"op": "insert", "parent": 1, "child": 4, "index": 1},
                {"op": "insert", "parent": 4, "child": 5, "index": 0},
                {"op": "listen", "id": 3, "events": ["click"]},
                {"op": "listen", "id": 5, "events": ["click"]}
            ]),
        );
        let behind = global(&d, w, 3);
        let ok = global(&d, w, 5);
        let at = center(&mut d, behind);
        let got = events(&click(&mut d, at));
        assert!(got.is_empty(), "blocked by the dialog: {got:?} at {at:?}");
        let at = center(&mut d, ok);
        assert_eq!(events(&click(&mut d, at)), [(5, "click".into(), Value::Null)]);
        assert_eq!(events(&key(&mut d, Key::Escape)), [(4, "cancel".into(), Value::Null)]);
    }

    #[test]
    fn split_dividers_drag_and_windows_resize_maximize_and_change_workspace() {
        let mut d = desk();
        let w = open(
            &mut d,
            7,
            json!([
                {"op": "create", "id": 2, "type": "split", "props": {"ratio": 0.5}},
                {"op": "create", "id": 3, "type": "vstack", "props": {}},
                {"op": "create", "id": 4, "type": "vstack", "props": {}},
                {"op": "insert", "parent": 1, "child": 2, "index": 0},
                {"op": "insert", "parent": 2, "child": 3, "index": 0},
                {"op": "insert", "parent": 2, "child": 4, "index": 1}
            ]),
        );
        let split = global(&d, w, 2);
        d.relayout();
        let (r, _) = d.scene.screen_rects()[&split];
        let divider = r.x + prop(&d, split, "_divider").as_f64().unwrap() as f32 + DIVIDER / 2.0;
        d.input(Input::MotionTo { x: divider, y: r.y + 50.0 });
        d.input(Input::Button { pressed: true });
        let out = d.input(Input::MotionTo { x: r.x + r.w * 0.25, y: r.y + 50.0 }).1;
        d.input(Input::Button { pressed: false });
        assert_eq!(events(&out), [(2, "change".into(), json!(0.25))]);

        // Resize from the corner grip: the app hears the final size once.
        let frame = d.apps.frame_of(w).unwrap();
        let f = d.scene.local_rect(frame).unwrap();
        let grip = (f.x + f.w - 4.0, f.y + f.h - 4.0);
        d.input(Input::MotionTo { x: grip.0, y: grip.1 });
        d.input(Input::Button { pressed: true });
        let moving = d.input(Input::MotionTo { x: grip.0 + 100.0, y: grip.1 + 50.0 }).1;
        assert!(moving.is_empty());
        let done = d.input(Input::Button { pressed: false }).1;
        assert_eq!(done.len(), 1);
        assert!(
            matches!(&done[0], Out::Part { data, .. } if data["kind"] == "resized" && data["width"] == 500)
        );

        // Ctrl+Alt+Up maximizes the top window.
        let out =
            d.input(Input::Key { key: Key::Up, mods: Mods { ctrl: true, alt: true, ..Mods::default() } }).1;
        assert!(matches!(&out[..], [Out::Part { data, .. }] if data["width"] == 1280));

        // Workspaces: moving the window to 2 hides it; switching shows it.
        let ctrl_alt = Mods { ctrl: true, alt: true, ..Mods::default() };
        d.input(Input::Key { key: Key::Char('@'), mods: Mods { shift: true, ..ctrl_alt } });
        assert_eq!(prop(&d, frame, "hidden"), true);
        d.input(Input::Key { key: Key::Right, mods: ctrl_alt });
        assert_eq!(d.workspace, 2);
        assert_eq!(prop(&d, frame, "hidden"), false);
        assert_eq!(d.apps.workspace, 2, "new windows open here");
    }

    #[test]
    fn the_switcher_shows_briefly_and_the_launcher_lists_and_launches_apps() {
        let mut d = desk();
        open(&mut d, 7, json!([]));
        open(&mut d, 20, json!([]));
        let effect = d.input(Input::Key { key: Key::Tab, mods: Mods { alt: true, ..Mods::default() } }).0;
        assert!(effect.raised.is_some());
        assert!(d.scene.node(SWITCHER).is_some());
        d.tick(Instant::now() + Duration::from_secs(2));
        assert!(d.scene.node(SWITCHER).is_none(), "gone after a moment");

        let out = click(&mut d, (20.0, 15.0));
        let [Out::Request { token, to, method, .. }] = &out[..] else { panic!("{out:?}") };
        assert_eq!((to.as_str(), method.as_str()), ("db", "query"));
        let docs = json!({"docs": [
            {"id": "a", "type": "App", "created": 1, "updated": 1, "data": {"id": "org.idealos.notes", "name": "Notes"}},
            {"id": "b", "type": "App", "created": 1, "updated": 1, "data": {"id": "org.idealos.counter", "name": "Counter"}}
        ]});
        d.call(Call::Reply { token: *token, result: Ok(docs) });
        assert!(d.scene.node(MENU).is_some());
        let out = key(&mut d, Key::Down);
        assert!(out.is_empty());
        let out = key(&mut d, Key::Enter);
        assert_eq!(
            out,
            [Out::Request {
                token: 0,
                to: "session".into(),
                method: "launch".into(),
                params: json!({"app": "org.idealos.notes"})
            }],
            "sorted by name: Counter, Notes"
        );
    }

    #[test]
    fn bound_text_fields_load_their_document_and_send_splices() {
        let mut d = desk();
        let out = d.call(request(7, "open_window", json!({"title": "Notes"}), true));
        let window = match &out[..] {
            [Out::Part { data, .. }] => data["window"].as_u64().unwrap(),
            other => panic!("{other:?}"),
        };
        let ops = json!([
            {"op": "create", "id": 2, "type": "text_area", "props": {"bind": {"doc": "d1", "path": "body.text"}, "w": 300, "h": 120}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0}
        ]);
        let out = d.call(request(8, "commit", json!({"window": window, "ops": ops}), false));
        let token = out
            .iter()
            .find_map(|o| match o {
                Out::Request { token, method, params, .. } if method == "get" && params["id"] == "d1" => {
                    assert_eq!(params["as"], "org.example.test", "read as the app");
                    Some(*token)
                }
                _ => None,
            })
            .expect("asks for the document");
        d.call(Call::Reply { token, result: Ok(json!({"id": "d1", "data": {"body": {"text": "héllo"}}})) });
        let area = global(&d, window, 2);
        assert_eq!(prop(&d, area, "_value"), "héllo");
        let at = center(&mut d, area);
        click(&mut d, at);
        key(&mut d, Key::End);
        let out = typed(&mut d, "!");
        let splice =
            out.iter().find(|o| matches!(o, Out::Request { method, .. } if method == "splice")).unwrap();
        assert_eq!(
            splice,
            &Out::Request {
                token: 0,
                to: "db".into(),
                method: "splice".into(),
                params: json!({"id": "d1", "path": "body.text", "at": 5, "delete": 0, "insert": "!", "as": "org.example.test"})
            },
            "positions in characters, written as the app"
        );
        let out = key(&mut d, Key::Enter);
        assert!(
            out.iter().any(|o| matches!(o, Out::Request { params, .. } if params["insert"] == "\n")),
            "a newline"
        );
    }

    #[test]
    fn images_load_their_blob_once_and_draw_its_pixels() {
        let mut d = desk();
        let mut png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png, 3, 2);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&[200u8; 18]).unwrap();
        }
        let hash = "ab".repeat(32);
        let out = d.call(request(7, "open_window", json!({"title": "Pics"}), true));
        let window = match &out[..] {
            [Out::Part { data, .. }] => data["window"].as_u64().unwrap(),
            other => panic!("{other:?}"),
        };
        let ops = json!([
            {"op": "create", "id": 2, "type": "vstack", "props": {}},
            {"op": "create", "id": 3, "type": "image", "props": {"blob": hash}},
            {"op": "create", "id": 4, "type": "image", "props": {"blob": hash}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0},
            {"op": "insert", "parent": 2, "child": 3, "index": 0},
            {"op": "insert", "parent": 2, "child": 4, "index": 1}
        ]);
        let out = d.call(request(8, "commit", json!({"window": window, "ops": ops}), false));
        let fetches: Vec<_> = out.iter().filter(|o| matches!(o, Out::FetchBlob { .. })).collect();
        let me = Some("org.example.test".to_owned());
        assert_eq!(
            fetches,
            [&Out::FetchBlob { hash: hash.clone(), app: me.clone() }],
            "once for both, as the app"
        );
        assert!(d.draw_list().layers.iter().all(|l| l.images.is_empty()), "a placeholder until it loads");
        d.call(Call::Blob { hash: hash.clone(), app: me.clone(), result: Ok(png) });
        let img = global(&d, window, 3);
        assert_eq!((prop(&d, img, "_iw"), prop(&d, img, "_ih")), (json!(3), json!(2)));
        let list = d.draw_list();
        let drawn: Vec<_> = list.layers.iter().flat_map(|l| &l.images).collect();
        assert_eq!(drawn.len(), 2);
        assert!(drawn.iter().all(|i| i.pixels.as_ref().is_some_and(|p| p.width == 3)));
        // A bad blob is logged and leaves the placeholder.
        d.call(Call::Blob { hash: "cd".repeat(32), app: me.clone(), result: Ok(b"junk".to_vec()) });
        d.call(Call::Blob { hash: "é".repeat(40), app: me.clone(), result: Err("gone".into()) });
        // A name that isn't a hash (here, one a byte-slicing log line would have
        // cut mid-character) is never fetched.
        let ops = json!([
            {"op": "create", "id": 5, "type": "image", "props": {"blob": "é".repeat(40)}},
            {"op": "create", "id": 6, "type": "image", "props": {"blob": "../../etc/passwd"}},
            {"op": "insert", "parent": 2, "child": 5, "index": 2},
            {"op": "insert", "parent": 2, "child": 6, "index": 3}
        ]);
        let out = d.call(request(9, "commit", json!({"window": window, "ops": ops}), false));
        assert!(!out.iter().any(|o| matches!(o, Out::FetchBlob { .. })), "{out:?}");
    }

    fn grant(d: &mut Desktop, version: i64, grants: Value) {
        d.call(Call::Grants(serde_json::from_value(json!({"version": version, "grants": grants})).unwrap()));
    }

    #[test]
    fn an_apps_images_show_only_once_it_may_read_them() {
        let mut d = desk();
        let mut png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png, 1, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&[9u8; 3]).unwrap();
        }
        let hash = "ef".repeat(32);
        let image = json!([{"op": "create", "id": 2, "type": "image", "props": {"blob": hash}}, {"op": "insert", "parent": 1, "child": 2, "index": 0}]);
        let a = open(&mut d, 10, image.clone());
        d.call(Call::Blob {
            hash: hash.clone(),
            app: Some("org.example.test".into()),
            result: Ok(png.clone()),
        });
        assert_eq!(prop(&d, global(&d, a, 2), "_iw"), json!(1));
        // Another app showing the same blob is checked on its own: the cached
        // pixels aren't shown until db says it may read it.
        let other = Some(Sender::App { app: "org.example.other".into(), conn: 4 });
        let out = d.call(Call::Request {
            id: 20,
            from: other.clone(),
            method: "open_window".into(),
            params: json!({"title": "B"}),
            stream: true,
        });
        let [Out::Part { data, .. }] = &out[..] else { panic!("{out:?}") };
        let b = data["window"].as_u64().unwrap();
        let out = d.call(Call::Request {
            id: 21,
            from: other,
            method: "commit".into(),
            params: json!({"window": b, "ops": image}),
            stream: false,
        });
        assert!(
            out.contains(&Out::FetchBlob { hash: hash.clone(), app: Some("org.example.other".into()) }),
            "{out:?}"
        );
        assert!(d.prop(global(&d, b, 2), "_iw").is_none(), "nothing shown yet");
        d.call(Call::Blob {
            hash: hash.clone(),
            app: Some("org.example.other".into()),
            result: Err("permission_denied".into()),
        });
        assert!(d.prop(global(&d, b, 2), "_iw").is_none(), "denied: still nothing");
        assert_eq!(prop(&d, global(&d, a, 2), "_iw"), json!(1), "the first app's stays");
        // When the first app's grants change, its image is checked again.
        grant(
            &mut d,
            3,
            json!([{"id": 1, "app": "org.example.test", "capability": "network", "granted": 0, "by": "t"}]),
        );
        assert_eq!(prop(&d, global(&d, a, 2), "_iw"), json!(0));
    }

    #[test]
    fn reading_the_clipboard_takes_a_grant() {
        let mut d = desk();
        d.call(request(1, "set_clipboard", json!({"text": "secret"}), false));
        let out = d.call(request(2, "get_clipboard", Value::Null, false));
        assert!(
            matches!(&out[..], [Out::Respond { result: Err(e), .. }] if e.code == "permission_denied"),
            "{out:?}"
        );
        grant(
            &mut d,
            1,
            json!([{"id": 1, "app": "org.example.test", "capability": "clipboard", "granted": 0, "by": "t"}]),
        );
        let out = d.call(request(3, "get_clipboard", Value::Null, false));
        assert_eq!(out, [Out::Respond { id: 3, result: Ok(json!({"text": "secret"})) }]);
        // An older table doesn't undo a newer one.
        grant(&mut d, 0, json!([]));
        assert!(matches!(
            &d.call(request(4, "get_clipboard", Value::Null, false))[..],
            [Out::Respond { result: Ok(_), .. }]
        ));
    }

    fn prompt_call(id: u64, from: Option<Sender>) -> Call {
        let params =
            json!({"app": "org.example.photos", "name": "Photos", "asks": ["read your Image documents"]});
        Call::Request { id, from, method: "prompt".into(), params, stream: false }
    }

    fn perm() -> Option<Sender> {
        Some(Sender::Service("perm".into()))
    }

    #[test]
    fn prompts_come_only_from_perm_and_take_all_input_until_answered() {
        let mut d = desk();
        let w = open(&mut d, 7, widgets());
        let out = d.call(prompt_call(1, app()));
        assert!(matches!(&out[..], [Out::Respond { result: Err(e), .. }] if e.code == "permission_denied"));
        assert!(d.scene.node(prompt::PROMPT).is_none());

        assert!(d.call(prompt_call(2, perm())).is_empty(), "waits for the user");
        assert!(d.call(prompt_call(3, perm())).is_empty(), "queued behind the first");
        // It's drawn over everything, the top bar included.
        let list = d.draw_list();
        let last = list.layers.last().unwrap();
        assert!(last.texts.iter().any(|t| t.text == "“Photos” would like to"), "the top layer");
        // Clicking a window under it does nothing.
        let button = global(&d, w, 3);
        let at = center(&mut d, button);
        let out = click(&mut d, at);
        assert!(out.is_empty(), "{out:?}");
        // Enter refuses (Don't allow has the focus); then Tab and Enter allow the next.
        let (_, out) = d.input(Input::Key { key: Key::Enter, mods: Mods::default() });
        assert_eq!(out, [Out::Respond { id: 2, result: Ok(json!({"allow": false})) }]);
        assert!(d.scene.node(prompt::PROMPT).is_some(), "the next one");
        d.input(Input::Key { key: Key::Tab, mods: Mods::default() });
        let (_, out) = d.input(Input::Key { key: Key::Enter, mods: Mods::default() });
        assert_eq!(out, [Out::Respond { id: 3, result: Ok(json!({"allow": true})) }]);
        assert!(d.scene.node(prompt::PROMPT).is_none());
        assert!(!click(&mut d, at).is_empty(), "windows hear presses again");
    }

    #[test]
    fn prompts_answer_to_presses_and_go_when_perm_gives_up() {
        let mut d = desk();
        d.call(prompt_call(5, perm()));
        let shown = d.shown_prompt.unwrap();
        let mid = |r: Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let (x, y) = mid(shown.allow);
        d.input(Input::MotionTo { x, y });
        let (_, out) = d.input(Input::Button { pressed: true });
        assert_eq!(out, [Out::Respond { id: 5, result: Ok(json!({"allow": true})) }]);
        d.call(prompt_call(6, perm()));
        d.call(Call::Cancel { id: 6 });
        assert!(d.scene.node(prompt::PROMPT).is_none(), "taken down");
        d.call(prompt_call(7, perm()));
        let (_, out) = d.input(Input::Key { key: Key::Escape, mods: Mods::default() });
        assert_eq!(out, [Out::Respond { id: 7, result: Ok(json!({"allow": false})) }]);
    }

    #[test]
    fn the_clipboard_is_shared_with_apps_and_bad_requests_get_errors() {
        let mut d = desk();
        grant(
            &mut d,
            1,
            json!([{"id": 1, "app": "org.example.test", "capability": "clipboard", "granted": 0, "by": "t"}]),
        );
        d.call(request(1, "set_clipboard", json!({"text": "copied", "doc": "d9"}), false));
        let out = d.call(request(2, "get_clipboard", Value::Null, false));
        assert_eq!(out, [Out::Respond { id: 2, result: Ok(json!({"text": "copied", "doc": "d9"})) }]);
        let err = |out: Vec<Out>| match &out[..] {
            [Out::Respond { result: Err(e), .. }] => e.message.clone(),
            other => panic!("{other:?}"),
        };
        assert!(err(d.call(request(3, "open_window", json!({"title": "x"}), false))).contains("streamed"));
        assert!(
            err(d.call(request(4, "commit", json!({"window": 5, "ops": []}), false))).contains("no window")
        );
        assert!(err(d.call(request(5, "explode", Value::Null, false))).contains("no method"));
        assert!(
            err(d.call(Call::Request {
                id: 6,
                from: None,
                method: "get_clipboard".into(),
                params: Value::Null,
                stream: false
            }))
            .contains("sender")
        );
    }
}
