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

use crate::apps::{Apps, Asked, Reply};
use crate::cursor::Shape;
use crate::demo::TOP_BAR;
use crate::draw::FIELD_PAD;
use crate::input::{Effect, Input, Key, Mods, Seat, Shortcut};
use crate::layout::{DIVIDER, Layout};
use crate::prompt;
use crate::scene::{ROOT, Rect, Scene, num};
use crate::shell::{self, BAR_HEIGHT, WORKSPACES};
use crate::terminal;
use crate::textedit::{Edit, Move, Splice};
use crate::texthistory::{Kind, TextHistory};
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
    /// The `appearance.theme` setting is now dark (or light).
    Theme {
        dark: bool,
    },
    /// The `appearance.reduce_motion` setting changed.
    ReduceMotion {
        on: bool,
    },
    /// The `StyleSheet` documents now: each target (`system`, `app:<id>`) and its
    /// edits, newest per target.
    StyleSheets {
        sheets: Vec<(String, String)>,
    },
    /// A screen frame for terminal node `node`, asked for with [`Out::Attach`].
    Screen {
        node: NodeId,
        frame: Value,
    },
    /// Terminal node `node`'s session ended (or couldn't be attached).
    ScreenEnd {
        node: NodeId,
    },
    /// A part of the live query following bound text field `node`'s document
    /// (a [`protocol::db::WatchEvent`]), asked for with [`Out::Follow`].
    Doc {
        node: NodeId,
        event: Value,
    },
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
    /// Attach terminal node `node` to its pty session (`params` for `attach`);
    /// frames come back as [`Call::Screen`].
    Attach { node: NodeId, params: Value },
    /// Follow bound text field `node`'s document with a live query (`params`
    /// for `db watch`); its parts come back as [`Call::Doc`].
    Follow { node: NodeId, params: Value },
    /// Stop the attachment for terminal node `node`, or the live query for
    /// bound text field `node`.
    Detach { node: NodeId },
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
    /// An edit to a bound text field's document, in its undo history's group.
    Splice { node: NodeId, group: u64 },
    /// Undoing one of a bound text field's change sets (Command-Z, or redo).
    Undo { node: NodeId, group: u64 },
    /// The installed apps, for the launcher menu.
    Launcher,
}

/// A bound text field following its document (see `Desktop::follows`).
#[derive(Default)]
struct Follow {
    in_flight: u32,
    held: Option<String>,
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
    /// Resizing an app window by `edges`, from where the pointer was (`from`), its
    /// content size then, and where its frame was (left and top edges move it).
    Resize {
        window: u64,
        frame: NodeId,
        edges: Edges,
        from: (f32, f32),
        size: (u32, u32),
        pos: (f32, f32),
        last: Option<Reply>,
    },
    /// Dragging a scroll view's thumb, `grab` pixels below its top.
    Thumb {
        node: NodeId,
        grab: f32,
    },
    /// The button that opened a menu or a select's list is still held: letting go
    /// over an item chooses it, if the pointer moved.
    Menu {
        from: (f32, f32),
    },
}

/// Which edges of a window a resize moves.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Edges {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Edges {
    fn cursor(self) -> Shape {
        match (self.left || self.right, self.top || self.bottom) {
            (true, false) => Shape::ResizeEw,
            (false, true) => Shape::ResizeNs,
            _ if (self.left && self.top) || (self.right && self.bottom) => Shape::ResizeNwse,
            _ => Shape::ResizeNesw,
        }
    }
}

/// How far inside and outside a window's edge the pointer can be and still resize
/// it (mostly outside, so a scrollbar at the edge stays usable), and how far along
/// an edge its corners reach.
const EDGE_IN: f32 = 3.0;
const EDGE_OUT: f32 = 5.0;
const CORNER: f32 = 16.0;
/// How far the pointer must move, holding the button that opened a list, before
/// letting go chooses the item under it.
const DRAG_TO_CHOOSE: f32 = 4.0;

/// What the pointer is over, for hover states and the cursor.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Over {
    Nothing,
    Edge(Edges),
    Control(NodeId),
    Divider(NodeId, bool),
    Thumb(NodeId),
    Title(u64),
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
/// How long the window manager's own motion takes: windows sliding in on a
/// workspace switch, a new window rising into place.
const MOVE: Duration = Duration::from_millis(180);
/// How far windows slide in when switching workspaces.
const SLIDE: f32 = 48.0;
/// How far below its place a new window starts.
const RISE: f32 = 16.0;
/// How far in from a scroll view's right edge its scrollbar takes the pointer.
const THUMB_ZONE: f32 = 12.0;

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
    /// Bound text fields following their documents, and for each, how many of
    /// its edits db hasn't answered yet and the newest text that arrived
    /// meanwhile. Text that arrives while edits are in flight can be older than
    /// what's shown, so it waits until they're answered.
    follows: HashMap<NodeId, Follow>,
    /// Bound text fields' undo histories (see `texthistory`).
    histories: HashMap<NodeId, TextHistory>,
    /// The caret and selection each text widget last told its app, as [anchor, caret].
    selects_told: HashMap<NodeId, (usize, usize)>,
    drag: Option<Drag>,
    /// The control the pointer is over, which shows its `:hover` style.
    hovered: Option<NodeId>,
    /// The app window whose title bar the pointer is over, and the one being
    /// dragged by it.
    hovered_title: Option<u64>,
    pressed_title: Option<u64>,
    /// The app window that's active (frontmost), whose title bar shows it.
    active_window: Option<u64>,
    /// The scroll view whose thumb is hovered or dragged.
    thumb: Option<NodeId>,
    /// The pointer image to show.
    cursor: Shape,
    /// What Ctrl-K last cut, for Ctrl-Y to put back (Emacs's kill ring, one deep).
    kill_ring: String,
    /// The pointer is a finger on a touchscreen, so dragging content scrolls it.
    /// No backend reports touch input yet.
    pub touch: bool,
    /// Each app window's content area: its app, and the sheet it opened with.
    window_styles: HashMap<NodeId, (Option<String>, Option<std::sync::Arc<crate::style::Sheet>>)>,
    /// Edits to apps' sheets (`StyleSheet` documents), by app id.
    style_edits: HashMap<String, std::sync::Arc<crate::style::Sheet>>,
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
    /// Terminal nodes' screens and sessions.
    terms: HashMap<NodeId, terminal::View>,
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
        let mut scene = Scene::new();
        // The permission prompt keeps the built-in look, whatever a sheet's edits do.
        scene.pin_builtin(prompt::PROMPT);
        Desktop {
            scene,
            wm: WindowManager::default(),
            seat: Seat::new(w, h),
            apps,
            layout: Layout::new(fonts),
            clipboard: Clipboard::default(),
            workspace: 1,
            focus: None,
            edits: HashMap::new(),
            follows: HashMap::new(),
            histories: HashMap::new(),
            selects_told: HashMap::new(),
            drag: None,
            hovered: None,
            hovered_title: None,
            pressed_title: None,
            active_window: None,
            thumb: None,
            cursor: Shape::Arrow,
            touch: false,
            kill_ring: String::new(),
            window_styles: HashMap::new(),
            style_edits: HashMap::new(),
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
            terms: HashMap::new(),
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
        self.size_terminals();
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
        self.mark_active();
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
                self.hover();
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
        // An app window's edges and corners resize it, just outside it included.
        if let Some((frame, edges)) = self.edge_at(x, y)
            && let Some(win) = self.apps.window_of(frame)
            && let Some(size) = self.apps.size_of(win)
            && let Some(r) = self.scene.local_rect(frame)
        {
            self.scene.raise(frame);
            self.restack();
            self.drag = Some(Drag::Resize {
                window: win,
                frame,
                edges,
                from: (x, y),
                size,
                pos: (r.x, r.y),
                last: None,
            });
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
                    self.drag = Some(Drag::Menu { from: (x, y) });
                }
                NodeKind::Menu => {
                    self.set_focus(Some(node));
                    self.open_actions(node, rect);
                    self.drag = Some(Drag::Menu { from: (x, y) });
                }
                NodeKind::Terminal => self.set_focus(Some(node)),
                NodeKind::TextField | NodeKind::TextArea => {
                    // Where the click is, before focusing: focusing a field puts
                    // its caret at the end and scrolls there, which in a long text
                    // would move what was clicked on.
                    let at = self.index_at(node, rect, x, y);
                    let mut e = Edit::new(&self.text_value(node));
                    e.move_caret(Move::To(at), false);
                    self.edits.insert(node, e);
                    self.set_focus(Some(node));
                    self.edit(node, |e| {
                        e.move_caret(Move::To(at), false);
                        None
                    });
                    self.drag = Some(Drag::Text(node));
                }
                NodeKind::Split => self.drag = Some(Drag::Split(node)),
                NodeKind::Scroll | NodeKind::Vlist => {
                    self.coasting.retain(|(n, _)| *n != node);
                    let p = &self.scene.node(node).expect("exists").props;
                    if x >= rect.x + rect.w - THUMB_ZONE
                        && let Some(t) = crate::draw::scroll_thumb(rect, p)
                    {
                        // On the thumb, drag it; on the track beside it, jump there first.
                        let grab = if (t.y..t.y + t.h).contains(&y) { y - t.y } else { t.h / 2.0 };
                        self.set(node, json!({"_thumb": "active"}));
                        self.thumb = Some(node);
                        self.drag = Some(Drag::Thumb { node, grab });
                        self.motion();
                    } else if self.touch {
                        // A finger drags content; a mouse selects and drags things instead.
                        self.drag = Some(Drag::Scroll { node, last: y, velocity: 0.0 });
                    }
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
        // Pressing an app window's title bar starts dragging it: it shows pressed.
        if let Some(win) = self.title_at(window, x, y) {
            self.set_title(win, "pressed", json!(true));
            self.pressed_title = Some(win);
        }
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
        if let Some(win) = self.pressed_title.take() {
            self.set_title(win, "pressed", Value::Null);
        }
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
                self.hover();
                Effect::default()
            }
            Some(Drag::Thumb { node, .. }) => {
                if self.scene.node(node).is_some() {
                    self.set(node, json!({"_thumb": Value::Null}));
                }
                self.thumb = None;
                self.hover();
                Effect::default()
            }
            Some(Drag::Menu { from }) => {
                // Held, dragged and let go over an item: that's the choice. A plain
                // click leaves the list open for another click to choose.
                let moved = (x - from.0).abs().max((y - from.1).abs()) > DRAG_TO_CHOOSE;
                if moved && self.menu.as_ref().is_some_and(|m| m.rect.contains(x, y)) {
                    self.menu_press(x, y);
                }
                Effect::default()
            }
            Some(_) => Effect::default(),
            None => self.seat.handle(Input::Button { pressed: false }, &mut self.scene, &mut self.wm),
        }
    }

    /// Mark the control under the pointer as hovered (`_hover`), and the last one
    /// not. While something is dragged, the hover stays where it was.
    fn hover(&mut self) {
        if self.drag.as_ref().is_some_and(|d| !matches!(d, Drag::Menu { .. })) {
            return;
        }
        let (x, y) = self.seat.cursor();
        // With a menu or a select's list open, the item under the pointer is the
        // highlighted one (the highlight glides there).
        if let Some(m) = &mut self.menu {
            let i = ((y - m.rect.y - 4.0) / ROW).floor();
            if m.rect.contains(x, y) && i >= 0.0 && (i as usize) < m.values.len() && i as usize != m.highlight
            {
                m.highlight = i as usize;
                self.set(MENU + 2, json!({"y": 4.0 + i * ROW}));
            }
            return;
        }
        let over = self.over(x, y);
        self.cursor = match over {
            Over::Edge(e) => e.cursor(),
            Over::Divider(_, true) => Shape::ResizeNs,
            Over::Divider(_, false) => Shape::ResizeEw,
            _ => Shape::Arrow,
        };
        // A control or a divider: `_hover`.
        let node = match over {
            Over::Control(n) | Over::Divider(n, _) => Some(n),
            _ => None,
        };
        if node != self.hovered {
            if let Some(old) = self.hovered.take()
                && self.scene.node(old).is_some()
            {
                self.set(old, json!({"_hover": Value::Null}));
            }
            if let Some(new) = node {
                self.set(new, json!({"_hover": true}));
            }
            self.hovered = node;
        }
        // A title bar.
        let title = match over {
            Over::Title(w) => Some(w),
            _ => None,
        };
        if title != self.hovered_title {
            if let Some(old) = self.hovered_title.take() {
                self.set_title(old, "_hover", Value::Null);
            }
            if let Some(new) = title {
                self.set_title(new, "_hover", json!(true));
            }
            self.hovered_title = title;
        }
        // A scroll view's thumb.
        let thumb = match over {
            Over::Thumb(n) => Some(n),
            _ => None,
        };
        if thumb != self.thumb {
            if let Some(old) = self.thumb.take()
                && self.scene.node(old).is_some()
            {
                self.set(old, json!({"_thumb": Value::Null}));
            }
            if let Some(new) = thumb {
                self.set(new, json!({"_thumb": "hover"}));
            }
            self.thumb = thumb;
        }
    }

    /// The pointer image to show now.
    pub fn cursor_shape(&self) -> Shape {
        match &self.drag {
            Some(Drag::Resize { edges, .. }) => edges.cursor(),
            Some(Drag::Split(n)) => {
                if self.prop(*n, "vertical").and_then(Value::as_bool) == Some(true) {
                    Shape::ResizeNs
                } else {
                    Shape::ResizeEw
                }
            }
            _ => self.cursor,
        }
    }

    /// What's under the pointer at (`x`, `y`), for hover states and the cursor.
    fn over(&mut self, x: f32, y: f32) -> Over {
        let control = |s: &Self, id: NodeId| {
            matches!(
                s.kind(id),
                Some(
                    NodeKind::Button
                        | NodeKind::Checkbox
                        | NodeKind::Toggle
                        | NodeKind::Select
                        | NodeKind::Menu
                        | NodeKind::Slider
                )
            )
        };
        if let Some((_, edges)) = self.edge_at(x, y) {
            return Over::Edge(edges);
        }
        let Some(window) = self.wm.window_at(&self.scene, x, y) else {
            let bar = self.scene.screen_rects().get(&TOP_BAR).map(|r| r.0);
            if bar.is_some_and(|r| r.contains(x, y))
                && let Some(n) = self.scene.hit(TOP_BAR, x, y, |id| control(self, id))
            {
                return Over::Control(n);
            }
            return Over::Nothing;
        };
        let root = self.dialog_in(window).unwrap_or(window);
        if let Some(n) = self.scene.hit(window, x, y, |id| control(self, id)) {
            return Over::Control(n);
        }
        for (id, rect, clip) in self.scene.paint_order(root).into_iter().rev() {
            if !rect.intersect(&clip).contains(x, y) {
                continue;
            }
            let p = &self.scene.node(id).expect("in the scene").props;
            match self.kind(id) {
                Some(NodeKind::Split) => {
                    let at = num(p, "_divider", -1.0);
                    let vertical = p.get("vertical").and_then(Value::as_bool) == Some(true);
                    let (pos, start) = if vertical { (y, rect.y) } else { (x, rect.x) };
                    if at >= 0.0 && pos >= start + at - 2.0 && pos <= start + at + DIVIDER + 2.0 {
                        return Over::Divider(id, vertical);
                    }
                }
                Some(NodeKind::Scroll | NodeKind::Vlist) if x >= rect.x + rect.w - THUMB_ZONE => {
                    if let Some(t) = crate::draw::scroll_thumb(rect, p)
                        && (t.y..t.y + t.h).contains(&y)
                    {
                        return Over::Thumb(id);
                    }
                }
                _ => {}
            }
        }
        match self.title_at(window, x, y) {
            Some(w) => Over::Title(w),
            None => Over::Nothing,
        }
    }

    /// The app window whose edge or corner the pointer at (`x`, `y`) would resize,
    /// and which edges: the topmost window whose resize zone holds the point. A
    /// window above it there covers it; a maximized window doesn't resize.
    fn edge_at(&self, x: f32, y: f32) -> Option<(NodeId, Edges)> {
        for &frame in self.scene.top_level().iter().rev() {
            if !wm::is_window(&self.scene, frame) {
                continue;
            }
            let Some(r) = self.scene.local_rect(frame) else { continue };
            if !r.expand(EDGE_OUT).contains(x, y) {
                continue;
            }
            let win = self.apps.window_of(frame)?;
            if self.apps.is_maximized(win) {
                return None;
            }
            // Distances inside the frame from each edge (negative: outside it).
            let (l, rt, t, b) = (x - r.x, r.x + r.w - x, y - r.y, r.y + r.h - y);
            let side = |d: f32| d < EDGE_IN;
            let corner = |d: f32| d < CORNER;
            let e = Edges {
                left: side(l) || (corner(l) && (side(t) || side(b))),
                right: side(rt) || (corner(rt) && (side(t) || side(b))),
                top: side(t) || (corner(t) && (side(l) || side(rt))),
                bottom: side(b) || (corner(b) && (side(l) || side(rt))),
            };
            return (e.left || e.right || e.top || e.bottom).then_some((frame, e));
        }
        None
    }

    /// The app window whose title bar is at (`x`, `y`) in frame `frame`.
    fn title_at(&self, frame: NodeId, x: f32, y: f32) -> Option<u64> {
        let win = self.apps.window_of(frame)?;
        let r = self.scene.local_rect(frame)?;
        (r.contains(x, y) && y < r.y + wm::TITLE_BAR_HEIGHT).then_some(win)
    }

    /// Set prop `key` on app window `window`'s title bar parts.
    fn set_title(&mut self, window: u64, key: &str, value: Value) {
        let Some(parts) = self.apps.title_parts(window) else { return };
        for part in parts {
            if self.scene.node(part).is_some() {
                self.set(part, json!({ key: value.clone() }));
            }
        }
    }

    /// Show which app window is active (the frontmost): its title bar is `:focus`.
    fn mark_active(&mut self) {
        let active = self.top_window().and_then(|f| self.apps.window_of(f));
        if active == self.active_window {
            return;
        }
        if let Some(old) = self.active_window.take() {
            self.set_title(old, "_active", Value::Null);
        }
        if let Some(new) = active {
            self.set_title(new, "_active", json!(true));
        }
        self.active_window = active;
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
            Drag::Resize { window, frame, edges, from, size, pos, last } => {
                let (dx, dy) = (x - from.0, y - from.1);
                let grow = |near: bool, far: bool, d: f32| {
                    if far {
                        d
                    } else if near {
                        -d
                    } else {
                        0.0
                    }
                };
                let w = (size.0 as f32 + grow(edges.left, edges.right, dx)).max(0.0) as u32;
                let h = (size.1 as f32 + grow(edges.top, edges.bottom, dy)).max(0.0) as u32;
                let reply = self.apps.resize(&mut self.scene, window, w, h).or(last);
                // The left and top edges move the window by what it actually grew,
                // so its other edges stay put even at the smallest size.
                if (edges.left || edges.top)
                    && let Some((aw, ah)) = self.apps.size_of(window)
                {
                    let nx = if edges.left { pos.0 + size.0 as f32 - aw as f32 } else { pos.0 };
                    let ny = if edges.top { pos.1 + size.1 as f32 - ah as f32 } else { pos.1 };
                    self.set(frame, json!({"x": nx, "y": ny}));
                }
                Drag::Resize { window, frame, edges, from, size, pos, last: reply }
            }
            Drag::Thumb { node, grab } => {
                if let Some(r) = rects(self, node) {
                    let p = &self.scene.node(node).expect("exists").props;
                    let content = num(p, "_content", 0.0);
                    if let Some(t) = crate::draw::scroll_thumb(r, p)
                        && r.h > t.h
                    {
                        let at = ((y - grab - r.y) / (r.h - t.h)).clamp(0.0, 1.0) * (content - r.h);
                        let now = num(p, "_scroll", 0.0);
                        self.scroll_by(node, at - now);
                    }
                }
                Drag::Thumb { node, grab }
            }
            Drag::Menu { from } => {
                self.drag = Some(Drag::Menu { from });
                self.hover();
                return;
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
                let font = self.font_of(id);
                // Exact through a view past what shows, which covers a scroll step;
                // estimated below that, so a long text isn't all laid out to scroll.
                let through = num(p, "_scroll", 0.0) + 2.0 * view.h;
                self.layout.text_height(id, &text, &font, Some((view.w - 2.0 * FIELD_PAD).max(1.0)), through)
                    + 12.0
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
                | NodeKind::TextArea
                | NodeKind::Terminal => return Some((id, kind, rect)),
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

    /// If app window `window` shows a dialog and the focus isn't in it, focus the
    /// dialog's first focusable widget: a dialog takes the window's input.
    fn focus_dialog(&mut self, window: u64) {
        let Some(frame) = self.apps.frame_of(window) else { return };
        let Some(d) = self.dialog_in(frame) else { return };
        let inside = |s: &Self, n: NodeId| s.scene.paint_order(d).iter().any(|(id, ..)| *id == n);
        if self.focus.is_some_and(|f| inside(self, f)) {
            return;
        }
        if let Some(&first) = self.focusable(d).first() {
            self.set_focus(Some(first));
        }
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
                                | NodeKind::Terminal
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
            let kind = Kind::of(&s);
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
                let group = self.histories.entry(node).or_default().edit(kind, Instant::now());
                self.request(Some(Pending::Splice { node, group }), "db", "splice", params);
                if let Some(f) = self.follows.get_mut(&node) {
                    f.in_flight += 1;
                }
            }
        }
        self.show_caret(node);
    }

    /// Undo (or redo) bound text field `node`'s newest group of edits, through
    /// db: each change set is undone with a new one, which redo undoes in turn.
    fn undo_text(&mut self, node: NodeId, redo: bool) {
        let Some(h) = self.histories.get_mut(&node) else { return };
        let Some((sets, group)) = (if redo { h.redo() } else { h.undo() }) else { return };
        for set in sets {
            let mut params = json!({"change_set": set});
            self.act_for(node, &mut params);
            self.request(Some(Pending::Undo { node, group }), "db", "undo", params);
        }
    }

    /// Do what an app asked of its widget `node`: select text in it, focus it,
    /// or add its own change sets to the widget's undo history.
    /// Focus moves only within the frontmost window, so an app behind can't take
    /// the keyboard.
    fn do_asked(&mut self, node: NodeId, asked: Asked) {
        if !matches!(self.kind(node), Some(NodeKind::TextField | NodeKind::TextArea)) {
            return;
        }
        match asked {
            Asked::Select(anchor, caret) => self.edit(node, |e| {
                e.select_range(anchor, caret);
                None
            }),
            Asked::Undoable(sets) => self.histories.entry(node).or_default().add(sets),
            Asked::Focus => {
                let mut at = Some(node);
                while let Some(n) = at {
                    if Some(n) == self.top_window() {
                        return self.set_focus(Some(node));
                    }
                    at = self.scene.parent(n);
                }
            }
        }
    }

    /// Put text widget `node`'s caret and selection on screen, scrolling to keep
    /// the caret in view.
    fn show_caret(&mut self, node: NodeId) {
        let Some(e) = self.edits.get(&node).cloned() else { return };
        let told = (e.anchor(), e.caret());
        if self.apps.listens_for_select(node) && self.selects_told.get(&node) != Some(&told) {
            self.selects_told.insert(node, told);
            self.event(node, "select", json!([told.0, told.1]));
        }
        let Some(rect) = self.scene.layout_rect(node).or_else(|| self.scene.local_rect(node)) else { return };
        let multiline = self.kind(node) == Some(NodeKind::TextArea);
        let font = self.font_of(node);
        let inner_w = (rect.w - 2.0 * FIELD_PAD).max(1.0);
        let width = multiline.then_some(inner_w);
        let shown = self.shown_text(node, e.text());
        let (cx, cy, ch) = self.layout.caret_at(node, &shown, &font, width, e.caret());
        let sel: Vec<Value> = e
            .selection()
            .map(|(a, b)| self.layout.selection_rects(node, &shown, &font, width, a, b))
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

    /// The font node `node`'s text is drawn in, from its style and props.
    fn font_of(&self, node: NodeId) -> crate::style::Font {
        let props = self.scene.node(node).map(|n| &n.props);
        crate::draw::text_font(&self.scene.style(node), props.unwrap_or(&Props::new()))
    }

    /// A text widget's text as it's drawn and measured: one bullet per character
    /// in a `secret` field, so carets and clicks line up with what's shown.
    fn shown_text(&self, node: NodeId, text: &str) -> String {
        if self.prop(node, "secret").and_then(Value::as_bool) == Some(true) {
            "•".repeat(text.chars().count())
        } else {
            text.to_owned()
        }
    }

    /// The character under (`x`, `y`) in text widget `node`, which is at `rect`.
    fn index_at(&mut self, node: NodeId, rect: Rect, x: f32, y: f32) -> usize {
        let text = self.shown_text(node, &self.text_value(node));
        let font = self.font_of(node);
        let p = &self.scene.node(node).expect("exists").props;
        let multiline = self.kind(node) == Some(NodeKind::TextArea);
        let inner_x = rect.x + FIELD_PAD;
        if multiline {
            let top = rect.y + 4.0 + 2.0 - num(p, "_scroll", 0.0);
            let width = (rect.w - 2.0 * FIELD_PAD).max(1.0);
            self.layout.index_at(node, &text, &font, Some(width), x - inner_x, y - top)
        } else {
            let scrolled = num(p, "_scroll_x", 0.0);
            self.layout.index_at(node, &text, &font, None, x - inner_x + scrolled, 1.0)
        }
    }

    fn text_key(&mut self, node: NodeId, key: Key, mods: Mods) {
        let multiline = self.kind(node) == Some(NodeKind::TextArea);
        let extend = mods.shift;
        // Words: Option (Alt) as on a Mac, or Ctrl as on a PC.
        let word = mods.alt || (mods.ctrl && !mods.logo);
        // Emacs keys, as macOS text fields take them: Ctrl and a letter, without
        // Command. Ctrl-C, Ctrl-X and Ctrl-V stay copy, cut and paste.
        if mods.ctrl
            && !mods.logo
            && !mods.alt
            && let Key::Char(c) = key
        {
            let moved = |m: Move| {
                move |e: &mut Edit| {
                    e.move_caret(m, extend);
                    None
                }
            };
            match c.to_ascii_lowercase() {
                'a' => return self.edit(node, moved(Move::LineStart)),
                'e' => return self.edit(node, moved(Move::LineEnd)),
                'f' => return self.edit(node, moved(Move::Right)),
                'b' => return self.edit(node, moved(Move::Left)),
                'n' => return self.text_key(node, Key::Down, Mods { shift: extend, ..Mods::default() }),
                'p' => return self.text_key(node, Key::Up, Mods { shift: extend, ..Mods::default() }),
                'd' => return self.edit(node, Edit::delete),
                'h' => return self.edit(node, Edit::backspace),
                'k' => {
                    let mut killed = None;
                    self.edit(node, |e| {
                        let (text, splice) = e.kill_line()?;
                        killed = Some(text);
                        Some(splice)
                    });
                    if let Some(text) = killed {
                        self.kill_ring = text;
                    }
                    return;
                }
                'y' => {
                    let text = self.kill_ring.clone();
                    if !text.is_empty() {
                        self.edit(node, |e| Some(e.insert(&text)));
                    }
                    return;
                }
                't' => return self.edit(node, Edit::transpose),
                _ => {}
            }
        }
        match key {
            // Command (Super on a PC keyboard) for the clipboard and select-all;
            // Ctrl works for the clipboard too.
            Key::Char(c)
                if mods.logo || (mods.ctrl && matches!(c.to_ascii_lowercase(), 'c' | 'x' | 'v' | 'z')) =>
            {
                match c.to_ascii_lowercase() {
                    // Command-Z undoes, Command-Shift-Z redoes (bound fields: db
                    // keeps their changes).
                    'z' => self.undo_text(node, mods.shift),
                    'a' => self.edit(node, |e| {
                        e.select_all();
                        None
                    }),
                    // A secret field's text never goes on the clipboard.
                    'c' | 'x' if self.prop(node, "secret").and_then(Value::as_bool) == Some(true) => {}
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
                }
            }
            // Ctrl with another letter types nothing (Option-letter types accents).
            Key::Char(_) if mods.ctrl => {}
            Key::Char(c) => self.edit(node, |e| Some(e.insert(&c.to_string()))),
            Key::Enter if multiline => self.edit(node, |e| Some(e.insert("\n"))),
            Key::Enter => {
                let text = self.text_value(node);
                self.event(node, "submit", json!(text));
            }
            // Command-Backspace deletes to the line's start; Option (or Ctrl) a word.
            Key::Backspace if mods.logo => self.edit(node, |e| e.delete_to(Move::LineStart)),
            Key::Backspace if word => self.edit(node, |e| e.delete_to(Move::WordLeft)),
            Key::Backspace => self.edit(node, Edit::backspace),
            Key::Delete if mods.logo => self.edit(node, |e| e.delete_to(Move::LineEnd)),
            Key::Delete if word => self.edit(node, |e| e.delete_to(Move::WordRight)),
            Key::Delete => self.edit(node, Edit::delete),
            // Command-Left and -Right: the line's ends.
            Key::Left => self.edit(node, |e| {
                let m = if mods.logo {
                    Move::LineStart
                } else if word {
                    Move::WordLeft
                } else {
                    Move::Left
                };
                e.move_caret(m, extend);
                None
            }),
            Key::Right => self.edit(node, |e| {
                let m = if mods.logo {
                    Move::LineEnd
                } else if word {
                    Move::WordRight
                } else {
                    Move::Right
                };
                e.move_caret(m, extend);
                None
            }),
            // Home and End: the line's ends; with Ctrl or Command, the whole text's.
            Key::Home => self.edit(node, |e| {
                e.move_caret(if mods.primary() { Move::Home } else { Move::LineStart }, extend);
                None
            }),
            Key::End => self.edit(node, |e| {
                e.move_caret(if mods.primary() { Move::End } else { Move::LineEnd }, extend);
                None
            }),
            // Option-Up and -Down: by paragraph.
            Key::Up if mods.alt => self.edit(node, |e| {
                e.move_caret(Move::ParagraphUp, extend);
                None
            }),
            Key::Down if mods.alt => self.edit(node, |e| {
                e.move_caret(Move::ParagraphDown, extend);
                None
            }),
            // Command-Up and -Down: the start and end of the text.
            Key::Up if mods.logo => self.edit(node, |e| {
                e.move_caret(Move::Home, extend);
                None
            }),
            Key::Down if mods.logo => self.edit(node, |e| {
                e.move_caret(Move::End, extend);
                None
            }),
            Key::Up | Key::Down | Key::PageUp | Key::PageDown if multiline => {
                let Some(e) = self.edits.get(&node).cloned() else { return };
                // Wrapped as drawn: a text area outside a layout container has
                // only its own rect.
                let Some(rect) = self.scene.layout_rect(node).or_else(|| self.scene.local_rect(node)) else {
                    return;
                };
                let font = self.font_of(node);
                let width = Some((rect.w - 2.0 * FIELD_PAD).max(1.0));
                let (cx, cy, ch) = self.layout.caret_at(node, e.text(), &font, width, e.caret());
                // Page Up and Down go a view's height, less a row to keep in sight.
                let page = ((rect.h - 8.0) - ch).max(ch);
                let ty = match key {
                    Key::Up => cy - ch / 2.0,
                    Key::Down => cy + ch * 1.5,
                    Key::PageUp => (cy - page).max(0.0) + ch / 2.0,
                    _ => cy + page + ch / 2.0,
                };
                let at = self.layout.index_at(node, e.text(), &font, width, cx, ty);
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
        // Command-Up maximizes, except in a text area, which takes it as the
        // start of its text (Ctrl-Alt-Up maximizes from anywhere).
        let in_text_area = self.focus.is_some_and(|f| self.kind(f) == Some(NodeKind::TextArea));
        if mods.logo && key == Key::Up && !in_text_area {
            self.maximize_top();
            return Effect::default();
        }
        if self.menu.is_some() {
            self.menu_key(key);
            return Effect::default();
        }
        // A focused terminal gets every other key, Tab and Escape included.
        if let Some(node) = self.focus.filter(|&n| self.kind(n) == Some(NodeKind::Terminal)) {
            self.terminal_key(node, key, mods);
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
        self.fade_in(MENU);
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
                json!({"w": rect.w, "h": rect.h, "color": "$surface", "radius": theme::radius::M, "border_color": "$border", "border_width": 1}),
            ),
            create(
                MENU + 2,
                NodeKind::Rect,
                json!({"x": 4, "y": 4.0 + highlight as f32 * ROW, "w": rect.w - 8.0, "h": ROW, "color": "$selection", "radius": theme::radius::S,
                       "transition": {"y": 80}}),
            ),
            insert(MENU, MENU + 1),
            insert(MENU, MENU + 2),
            insert(ROOT, MENU),
        ];
        for (i, label) in labels.iter().enumerate() {
            let id = MENU + 10 + i as NodeId;
            ops.push(create(id, NodeKind::Text, json!({"x": 12, "y": 4.0 + i as f32 * ROW + 5.0, "w": rect.w - 24.0, "h": 18, "text": label, "size": 14, "color": "$text"})));
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
        let from = self.workspace;
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
                // The windows that appear slide in from the side being moved to.
                if !hidden && from != self.workspace {
                    let x = self.prop(id, "x").and_then(Value::as_f64).unwrap_or(0.0) as f32;
                    let dx = if self.workspace > from { SLIDE } else { -SLIDE };
                    self.scene.animate_from(
                        id,
                        "x",
                        crate::anim::Val::Num(x + dx),
                        MOVE,
                        crate::anim::Ease::Out,
                    );
                }
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
        let showing = self.switcher_until.is_some();
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
                json!({"w": w, "h": h, "color": "$surface", "radius": theme::radius::L, "border_color": "$border", "border_width": 1}),
            ),
            insert(SWITCHER, SWITCHER + 1),
            insert(ROOT, SWITCHER),
        ];
        for (i, (id, title)) in windows.iter().enumerate() {
            let y = 8.0 + i as f32 * ROW;
            if Some(*id) == raised {
                ops.push(create(SWITCHER + 2, NodeKind::Rect, json!({"x": 8, "y": y, "w": w - 16.0, "h": ROW, "color": "$selection", "radius": theme::radius::S})));
                ops.push(insert(SWITCHER, SWITCHER + 2));
            }
            let t = SWITCHER + 10 + i as NodeId;
            ops.push(create(t, NodeKind::Text, json!({"x": 18, "y": y + 5.0, "w": w - 36.0, "h": 18, "text": title, "size": 14, "color": "$text"})));
            ops.push(insert(SWITCHER, t));
        }
        self.scene.apply_all(ops);
        self.switcher_until = Some(Instant::now() + SWITCHER_SHOWS);
        if !showing {
            self.fade_in(SWITCHER);
        }
    }

    // --- Time ----------------------------------------------------------------------

    /// Move animations along to `now`: transitions, coasting scroll views, the
    /// switcher's timeout, the clock. Returns whether something is still animating.
    /// The KMS backend passes when the frame will reach the screen (the next vblank).
    pub fn tick(&mut self, now: Instant) -> bool {
        let moving = self.scene.tick(now);
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
        moving || !self.coasting.is_empty()
    }

    /// Edits to style sheets arrived: apply each that parses, keeping the last good
    /// one for a target whose new edits don't.
    fn take_style_sheets(&mut self, sheets: Vec<(String, String)>) {
        let parse = |target: &str, css: &str| -> Option<Option<std::sync::Arc<crate::style::Sheet>>> {
            if css.trim().is_empty() {
                return Some(None);
            }
            match crate::style::Sheet::parse(css) {
                Ok(s) => Some(Some(std::sync::Arc::new(s))),
                Err(e) => {
                    // Expected mid-edit (each keystroke arrives); the editors show where it is.
                    log::debug!("style: edits to {target} not applied: {e}");
                    None
                }
            }
        };
        let system = sheets.iter().find(|(t, _)| t == "system");
        match system.map(|(t, css)| parse(t, css)) {
            Some(Some(edits)) => self.scene.set_system_edits(edits),
            Some(None) => {}
            None => self.scene.set_system_edits(None),
        }
        let mut apps = HashMap::new();
        for (target, css) in &sheets {
            let Some(app) = target.strip_prefix("app:") else { continue };
            match parse(target, css) {
                Some(Some(sheet)) => {
                    apps.insert(app.to_owned(), sheet);
                }
                Some(None) => {}
                None => {
                    if let Some(old) = self.style_edits.get(app) {
                        apps.insert(app.to_owned(), old.clone());
                    }
                }
            }
        }
        self.style_edits = apps;
        self.window_styles.retain(|content, _| self.scene.node(*content).is_some());
        let windows: Vec<NodeId> = self.window_styles.keys().copied().collect();
        for content in windows {
            self.restyle_window(content);
        }
    }

    /// Give window content area `content` its app's sheet: the one it opened with,
    /// then any edits.
    fn restyle_window(&mut self, content: NodeId) {
        let Some((app, own)) = self.window_styles.get(&content) else { return };
        let edits = app.as_ref().and_then(|a| self.style_edits.get(a));
        let sheet = match (own, edits) {
            (Some(own), Some(e)) => Some(std::sync::Arc::new(own.then(e))),
            (Some(own), None) => Some(own.clone()),
            (None, Some(e)) => Some(e.clone()),
            (None, None) => None,
        };
        self.scene.set_app_style(content, sheet);
    }

    /// Fade node `root` and everything in it in: each colour prop from transparent
    /// to its colour. For the compositor's own menus and switcher, which are plain
    /// rects and text (a whole window can't fade until windows are textures).
    fn fade_in(&mut self, root: NodeId) {
        let ids: Vec<NodeId> = self.scene.paint_order(root).into_iter().map(|(id, ..)| id).collect();
        for id in ids {
            for prop in ["color", "text_color", "border_color"] {
                let Some(text) = self.prop(id, prop).and_then(Value::as_str).map(str::to_owned) else {
                    continue;
                };
                if let Some(c) = self.scene.style(id).resolve(&text) {
                    let clear = crate::scene::Rgba([c.0[0], c.0[1], c.0[2], 0]);
                    self.scene.animate_from(
                        id,
                        prop,
                        crate::anim::Val::Color(clear),
                        crate::anim::STATE_FADE,
                        crate::anim::Ease::Out,
                    );
                }
            }
        }
    }

    /// When [`Desktop::tick`] next has something to do.
    pub fn next_wake(&self, now: Instant) -> Instant {
        if !self.coasting.is_empty() || self.scene.animating() {
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
            Call::Theme { dark } => {
                log::info!("desktop: {} theme", if dark { "dark" } else { "light" });
                self.scene.set_dark(dark);
            }
            Call::StyleSheets { sheets } => self.take_style_sheets(sheets),
            Call::ReduceMotion { on } => {
                log::info!("desktop: reduce motion {}", if on { "on" } else { "off" });
                self.scene.set_reduce_motion(on);
            }
            Call::Screen { node, frame } => self.screen_frame(node, frame),
            Call::ScreenEnd { node } => {
                if self.terms.remove(&node).is_some() && self.scene.node(node).is_some() {
                    self.set(node, json!({"_cursor": Value::Null}));
                }
            }
            Call::Cancel { id } => {
                if self.apps.close_request(&mut self.scene, id) {
                    log::info!("apps: closed a window (request {id} ended)");
                    self.forget_gone();
                }
            }
            Call::Doc { node, event } => self.doc_changed(node, event),
            Call::Reply { token, result } => self.reply(token, result),
            Call::Blob { hash, app, result } => self.blob_loaded(hash, app, result),
            Call::Request { id, from, method, params, stream } => {
                self.request_in(id, from, &method, params, stream)
            }
        }
        // Lay out what the call changed, so what depends on layout (a virtual
        // list's visible rows) is sent now, not after some later call or input.
        self.relayout();
        self.drain()
    }

    /// Drop state for nodes that no longer exist.
    fn forget_gone(&mut self) {
        let gone: Vec<NodeId> =
            self.terms.keys().copied().filter(|n| self.scene.node(*n).is_none()).collect();
        for node in gone {
            self.terms.remove(&node);
            self.outbox.push(Out::Detach { node });
        }
        let gone: Vec<NodeId> =
            self.follows.keys().copied().filter(|n| self.scene.node(*n).is_none()).collect();
        for node in gone {
            self.follows.remove(&node);
            self.outbox.push(Out::Detach { node });
        }
        let scene = &self.scene;
        self.edits.retain(|n, _| scene.node(*n).is_some());
        self.histories.retain(|n, _| scene.node(*n).is_some());
        self.selects_told.retain(|n, _| scene.node(*n).is_some());
        self.layout.forget(|n| scene.node(n).is_none());
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
                    // The app's own style sheet, if it sent one: a mistake fails the
                    // request, so the app's author sees where it is.
                    let sheet = match p.style.as_deref().map(crate::style::Sheet::parse) {
                        Some(Err(e)) => return self.outbox.push(fail(format!("style: {e}"))),
                        Some(Ok(sheet)) => Some(std::sync::Arc::new(sheet)),
                        None => None,
                    };
                    // The broker's word on which app this is; an app can't claim to be
                    // another. Only a connection with no app identity (a tool, or an
                    // app run by hand on a Mac) is taken at its word, for styling.
                    let app_id = match &from {
                        Sender::App { app, .. } => Some(app.clone()),
                        _ => p.app.clone(),
                    };
                    let screen = self.seat.bounds();
                    // One line per window; tools/smoke-checks.ts counts them.
                    log::info!("apps: opened a window for {from:?}: {:?}", p.title);
                    let reply = self.apps.open(&mut self.scene, from, id, &p, screen);
                    // A new window rises into place.
                    if let protocol::WindowEvent::Opened { window, .. } = &reply.event
                        && let Some(frame) = self.apps.frame_of(*window)
                    {
                        let y = self.prop(frame, "y").and_then(Value::as_f64).unwrap_or(0.0) as f32;
                        self.scene.animate_from(
                            frame,
                            "y",
                            crate::anim::Val::Num(y + RISE),
                            MOVE,
                            crate::anim::Ease::Out,
                        );
                    }
                    if let protocol::WindowEvent::Opened { window, .. } = &reply.event
                        && let Some(content) = self.apps.content_of(*window)
                    {
                        self.window_styles.insert(content, (app_id, sheet));
                        self.restyle_window(content);
                    }
                    self.restack();
                    self.send(Some(reply));
                }
                Err(e) => self.outbox.push(fail(e)),
            },
            method::COMMIT => {
                let window = params.get("window").and_then(Value::as_u64);
                let result =
                    parse::<Commit>(params).and_then(|c| self.apps.commit(&mut self.scene, &from, c));
                match result {
                    Ok(created) => {
                        self.forget_gone();
                        self.load_bound(&created);
                        for (node, asked) in self.apps.take_asked() {
                            self.do_asked(node, asked);
                        }
                        if let Some(w) = window {
                            self.focus_dialog(w);
                        }
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
            method::CHECK_STYLE => {
                let result = parse::<protocol::scene::CheckStyleParams>(params).map(|p| {
                    let error =
                        crate::style::Sheet::parse(&p.css).err().map(|e| protocol::scene::StyleError {
                            line: e.line as u32,
                            col: e.col as u32,
                            message: e.message,
                        });
                    json!(protocol::scene::CheckStyleResult { error })
                });
                self.outbox.push(Out::Respond {
                    id,
                    result: result.map_err(|e| BusError { code: code::BAD_REQUEST.into(), message: e }),
                });
            }
            method::SYSTEM_STYLE => {
                let css = json!(protocol::scene::SystemStyleResult { css: crate::style::SYSTEM.into() });
                self.outbox.push(Out::Respond { id, result: Ok(css) });
            }
            method::SCREEN => {
                let (w, h) = self.seat.bounds();
                let info = protocol::scene::ScreenInfo {
                    width: w as u32,
                    height: h as u32,
                    workspaces: WORKSPACES,
                    workspace: self.workspace,
                };
                self.outbox
                    .push(Out::Respond { id, result: Ok(serde_json::to_value(info).expect("serializes")) });
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
            if self.kind(node) == Some(NodeKind::Terminal) {
                self.attach_terminal(node);
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

    // --- Terminals ---------------------------------------------------------------

    /// Attach new terminal node `node` to the pty session it names, as its app.
    fn attach_terminal(&mut self, node: NodeId) {
        let Some(session) = self.prop(node, "pty").and_then(Value::as_str).map(str::to_owned) else { return };
        let font = self.font_of(node);
        let cell = self.layout.cell(&font);
        self.set(node, json!({"_cell": [cell.0, cell.1]}));
        let view = terminal::View::new(session, self.apps.app_of(node).map(str::to_owned));
        let params = view.params(json!({}));
        self.terms.insert(node, view);
        self.outbox.push(Out::Attach { node, params });
    }

    fn screen_frame(&mut self, node: NodeId, frame: Value) {
        let Ok(frame) = serde_json::from_value::<protocol::pty::ScreenFrame>(frame) else { return };
        let Some(view) = self.terms.get_mut(&node) else { return };
        view.apply(frame);
        let props = view.props();
        if self.scene.node(node).is_some() {
            self.set(node, props);
        }
    }

    /// Tell each terminal's session how many cells now fit, if that changed.
    fn size_terminals(&mut self) {
        let nodes: Vec<NodeId> = self.terms.keys().copied().collect();
        let rects = self.scene.screen_rects();
        for node in nodes {
            let Some(&(rect, _)) = rects.get(&node) else { continue };
            let cell = self.prop(node, "_cell").and_then(Value::as_array).map(|c| {
                let n = |i: usize| c.get(i).and_then(Value::as_f64).unwrap_or(8.0) as f32;
                (n(0), n(1))
            });
            let fits = terminal::fit(rect.w, rect.h, cell.unwrap_or((8.0, 18.0)));
            let view = self.terms.get_mut(&node).expect("listed");
            if view.asked != Some(fits) {
                view.asked = Some(fits);
                let params = view.params(json!({"cols": fits.0, "rows": fits.1}));
                self.request(None, "pty", "resize", params);
            }
        }
    }

    /// Key `key` typed into terminal node `node`: bytes for its shell, or a paste
    /// (Command+V, or Ctrl+Shift+V, since Ctrl+V is the shell's).
    fn terminal_key(&mut self, node: NodeId, key: Key, mods: Mods) {
        let Some(view) = self.terms.get(&node) else { return };
        let paste = key == Key::Char('v') || key == Key::Char('V');
        let data = if paste && (mods.logo || (mods.ctrl && mods.shift)) {
            view.paste(&self.clipboard.text)
        } else if mods.logo {
            return; // Command shortcuts aren't the shell's
        } else {
            match terminal::key_bytes(key, mods, view.app_cursor) {
                Some(d) => d,
                None => return,
            }
        };
        if data.is_empty() {
            return;
        }
        let params = view.params(json!({"data": data}));
        self.request(None, "pty", "write", params);
    }

    /// A part of the live query following bound field `node`'s document.
    fn doc_changed(&mut self, node: NodeId, event: Value) {
        use protocol::db::{Doc, WatchEvent};
        let doc: Option<Doc> = match serde_json::from_value::<WatchEvent>(event) {
            Ok(WatchEvent::Initial { docs }) => docs.into_iter().next(),
            Ok(WatchEvent::Diff { added, changed, .. }) => added.into_iter().chain(changed).next(),
            // Deleted: leave the text as it was, to type into or copy from.
            Err(_) => None,
        };
        let Some(doc) = doc else { return };
        let Some(path) = self.prop(node, "bind").and_then(|b| b.get("path")).and_then(Value::as_str) else {
            return;
        };
        let mut at = None::<&Value>;
        for (i, key) in path.split('.').enumerate() {
            at = if i == 0 { doc.data.get(key) } else { at.and_then(|v| v.get(key)) };
        }
        let text = at.and_then(Value::as_str).unwrap_or_default().to_owned();
        let Some(f) = self.follows.get_mut(&node) else { return };
        if f.in_flight > 0 {
            f.held = Some(text);
        } else {
            self.take_bound_text(node, &text);
        }
    }

    /// Show `text`, which bound field `node`'s document now holds, if it isn't
    /// what's shown already (an echo of the field's own edits is), keeping the
    /// caret and selection where they were, and tell the app.
    fn take_bound_text(&mut self, node: NodeId, text: &str) {
        if self.scene.node(node).is_none() || self.text_value(node) == text {
            return;
        }
        self.set(node, json!({"_value": text}));
        if let Some(e) = self.edits.get_mut(&node) {
            e.follow(text);
        }
        self.apps.reported(node, text);
        self.event(node, "change", json!(text));
        if self.focus == Some(node) {
            self.show_caret(node);
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
                // From now on, follow the document: another window, a restore or
                // any other writer changes the shown text too.
                if let (Some(id), Some(doc_type)) =
                    (doc.get("id").and_then(Value::as_str), doc.get("type").and_then(Value::as_str))
                    && !self.follows.contains_key(&node)
                {
                    let mut params = json!({"type": doc_type, "where": {"$id": id}});
                    self.act_for(node, &mut params);
                    self.follows.insert(node, Follow::default());
                    self.outbox.push(Out::Follow { node, params });
                }
            }
            (Pending::Undo { node, group }, result) => match result {
                Ok(v) => {
                    if let (Some(h), Some(set)) = (self.histories.get_mut(&node), v["change_set"].as_i64()) {
                        h.record(group, set);
                    }
                }
                Err(e) => {
                    log::warn!("couldn't undo a bound field's edit: {}: {}", e.code, e.message);
                    if let Some(h) = self.histories.get_mut(&node) {
                        h.forget(group);
                    }
                }
            },
            (Pending::Splice { node, group }, result) => {
                match &result {
                    Ok(v) => {
                        if let (Some(h), Some(set)) =
                            (self.histories.get_mut(&node), v["change_set"].as_i64())
                        {
                            h.record(group, set);
                        }
                    }
                    Err(e) => {
                        log::warn!("couldn't save a bound field's edit: {}: {}", e.code, e.message);
                        if let Some(h) = self.histories.get_mut(&node) {
                            h.forget(group);
                        }
                    }
                }
                let Some(f) = self.follows.get_mut(&node) else { return };
                f.in_flight = f.in_flight.saturating_sub(1);
                if f.in_flight == 0
                    && let Some(text) = f.held.take()
                {
                    self.take_bound_text(node, &text);
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
        // Answered; events from laying out what it made (a list's rows) may follow.
        assert!(out.iter().any(|o| matches!(o, Out::Respond { result: Ok(_), .. })), "{out:?}");
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

    /// A long text area placed by its props (outside a layout container): a click
    /// puts the caret where it lands without scrolling, and Up and Down move by
    /// rows wrapped as they're drawn.
    #[test]
    fn long_text_areas_keep_the_click_and_move_by_drawn_rows() {
        let mut d = desk();
        let para = "the quick brown fox jumps over the lazy dog, again and again and again";
        let text = vec![para; 60].join("\n");
        let w = open(
            &mut d,
            7,
            json!([
                {"op": "create", "id": 2, "type": "text_area", "props": {"x": 10, "y": 10, "w": 300, "h": 200, "value": text}},
                {"op": "insert", "parent": 1, "child": 2, "index": 0}
            ]),
        );
        let area = global(&d, w, 2);
        let at = center(&mut d, area);
        click(&mut d, at);
        let caret = |d: &Desktop| {
            let c = prop(d, area, "_caret");
            (c[0].as_f64().unwrap() as f32, c[1].as_f64().unwrap() as f32, c[2].as_f64().unwrap() as f32)
        };
        let (_, y, row) = caret(&d);
        assert_eq!(
            prop(&d, area, "_scroll").as_f64().unwrap_or(0.0),
            0.0,
            "the click didn't scroll to the end"
        );
        assert!((y - (200.0 / 2.0 - 6.0)).abs() <= row, "the caret is on the row clicked: {y}");
        // From the start, Down goes one drawn row down; Up comes back.
        d.input(Input::Key { key: Key::Home, mods: Mods { ctrl: true, ..Mods::default() } });
        assert_eq!(caret(&d).1, 0.0);
        key(&mut d, Key::Down);
        let (x, y, _) = caret(&d);
        assert!((y - row).abs() < 1.0, "one row down: {y} (rows are {row})");
        assert!(x < 60.0, "near the row's start: {x}");
        key(&mut d, Key::Up);
        assert_eq!(caret(&d).1, 0.0);
        // Page Down goes most of a view down.
        key(&mut d, Key::PageDown);
        let (_, y, _) = caret(&d);
        assert!(y >= 200.0 - 8.0 - 2.0 * row && y <= 200.0, "about a view down: {y} (rows are {row})");
        // Option-Down: the paragraph's end; Option-Up: its start, then the one before's.
        let alt = Mods { alt: true, ..Mods::default() };
        let at = |d: &Desktop| d.edits[&area].caret();
        let chars: Vec<char> = d.edits[&area].text().chars().collect();
        d.input(Input::Key { key: Key::Down, mods: alt });
        assert_eq!(chars[at(&d)], '\n', "at a paragraph's end");
        d.input(Input::Key { key: Key::Up, mods: alt });
        let start = at(&d);
        assert!(start > 0 && chars[start - 1] == '\n', "at its start");
        d.input(Input::Key { key: Key::Up, mods: alt });
        assert_eq!(at(&d), start - para.chars().count() - 1, "the paragraph before");
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
        // Dragging content scrolls it on a touchscreen only.
        d.touch = true;
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
        // The commit's own answer carries the rows to make.
        let out = d.call(request(8, "commit", json!({"window": window, "ops": ops}), false));
        let first = events(&out);
        assert_eq!(first, [(2, "range".into(), json!([0, 4]))]);
        let vlist = global(&d, window, 2);
        let at = center(&mut d, vlist);
        d.input(Input::MotionTo { x: at.0, y: at.1 });
        let out = d.input(Input::Scroll { dx: 0.0, dy: 210.0 }).1;
        assert_eq!(events(&out), [(2, "range".into(), json!([10, 15]))]);
    }

    #[test]
    fn a_virtual_list_whose_count_comes_later_reports_rows_at_once() {
        // An app's list of what it hasn't loaded yet starts empty; the rows to
        // make must follow the count, with no input or other call needed.
        let mut d = desk();
        let ops = json!([
            {"op": "create", "id": 2, "type": "vlist", "props": {"row_height": 20, "h": 100}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0}
        ]);
        let out = d.call(request(7, "open_window", json!({"title": "List"}), true));
        let window = match &out[..] {
            [Out::Part { data, .. }] => data["window"].as_u64().unwrap(),
            other => panic!("{other:?}"),
        };
        assert!(
            events(&d.call(request(8, "commit", json!({"window": window, "ops": ops}), false))).is_empty()
        );
        let set = json!([{"op": "set", "id": 2, "props": {"count": 3}}]);
        let out = d.call(request(9, "commit", json!({"window": window, "ops": set}), false));
        assert_eq!(events(&out), [(2, "range".into(), json!([0, 2]))]);
    }

    #[test]
    fn a_virtual_lists_rows_span_the_list() {
        // A list sized only by growing, and a row that is a stack: a spacer that
        // grows pushes the row's last label to the list's right edge.
        let mut d = desk();
        let w = open(
            &mut d,
            7,
            json!([
                {"op": "create", "id": 2, "type": "vstack", "props": {"grow": 1}},
                {"op": "create", "id": 3, "type": "vlist", "props": {"count": 1, "row_height": 30, "grow": 1}},
                {"op": "create", "id": 4, "type": "hstack", "props": {"index": 0}},
                {"op": "create", "id": 5, "type": "label", "props": {"text": "Title"}},
                {"op": "create", "id": 6, "type": "label", "props": {"text": "", "grow": 1}},
                {"op": "create", "id": 7, "type": "label", "props": {"text": "3:59"}},
                {"op": "insert", "parent": 1, "child": 2, "index": 0},
                {"op": "insert", "parent": 2, "child": 3, "index": 0},
                {"op": "insert", "parent": 3, "child": 4, "index": 0},
                {"op": "insert", "parent": 4, "child": 5, "index": 0},
                {"op": "insert", "parent": 4, "child": 6, "index": 1},
                {"op": "insert", "parent": 4, "child": 7, "index": 2}
            ]),
        );
        d.relayout();
        let rects = d.scene.screen_rects();
        let (list, row, time) =
            (rects[&global(&d, w, 3)].0, rects[&global(&d, w, 4)].0, rects[&global(&d, w, 7)].0);
        assert!(list.w > 300.0, "{list:?}");
        assert_eq!((row.w, row.h), (list.w, 30.0));
        assert!((time.x + time.w - (list.x + list.w)).abs() < 1.0, "time {time:?} in list {list:?}");
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

        // Resize from the bottom-right corner: the app hears the final size once.
        let frame = d.apps.frame_of(w).unwrap();
        let f = d.scene.local_rect(frame).unwrap();
        let grip = (f.x + f.w - 1.0, f.y + f.h - 1.0);
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
        let Out::Request { to, params, .. } = splice else { unreachable!() };
        assert_eq!(
            (to.as_str(), params),
            (
                "db",
                &json!({"id": "d1", "path": "body.text", "at": 5, "delete": 0, "insert": "!", "as": "org.example.test"})
            ),
            "positions in characters, written as the app"
        );
        let out = key(&mut d, Key::Enter);
        assert!(
            out.iter().any(|o| matches!(o, Out::Request { params, .. } if params["insert"] == "\n")),
            "a newline"
        );
    }

    /// A bound field follows its document: another writer's change shows, with
    /// the caret kept on its character; the field's own echoes, and anything that
    /// arrives while its edits are in flight, wait until db has answered them.
    #[test]
    fn bound_text_fields_follow_their_document() {
        let mut d = desk();
        let out = d.call(request(7, "open_window", json!({"title": "Notes"}), true));
        let [Out::Part { data, .. }] = &out[..] else { panic!("{out:?}") };
        let window = data["window"].as_u64().unwrap();
        let ops = json!([
            {"op": "create", "id": 2, "type": "text_area", "props": {"bind": {"doc": "d1", "path": "body.text"}, "w": 300, "h": 120}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0}
        ]);
        let out = d.call(request(8, "commit", json!({"window": window, "ops": ops}), false));
        let get = out.iter().find_map(|o| match o {
            Out::Request { token, method, .. } if method == "get" => Some(*token),
            _ => None,
        });
        let doc = |text: &str| json!({"id": "d1", "type": "Note", "created": 1, "updated": 2, "data": {"body": {"text": text}}});
        let diff = |text: &str| json!({"kind": "diff", "added": [], "changed": [doc(text)], "removed": []});
        let out = d.call(Call::Reply { token: get.unwrap(), result: Ok(doc("héllo")) });
        let area = global(&d, window, 2);
        assert_eq!(
            out.iter().filter(|o| matches!(o, Out::Follow { .. })).collect::<Vec<_>>(),
            [&Out::Follow {
                node: area,
                params: json!({"type": "Note", "where": {"$id": "d1"}, "as": "org.example.test"})
            }],
            "a live query for the one document, as the app"
        );
        // The first part is what get said: nothing to do.
        let out =
            d.call(Call::Doc { node: area, event: json!({"kind": "initial", "docs": [doc("héllo")]}) });
        assert!(!out.iter().any(|o| matches!(o, Out::Part { .. })), "{out:?}");

        // Another writer adds text before the caret: it shows, the caret moves along.
        let at = center(&mut d, area);
        click(&mut d, at);
        key(&mut d, Key::End);
        let out = d.call(Call::Doc { node: area, event: diff("well, héllo") });
        assert_eq!(prop(&d, area, "_value"), "well, héllo");
        assert_eq!(d.edits[&area].caret(), 11);
        assert!(
            out.iter().any(|o| matches!(o, Out::Part { data, .. } if data["event"] == "change" && data["value"] == "well, héllo")),
            "the app hears of it: {out:?}"
        );

        // Typing sends a splice. Parts that arrive before db answers it (an
        // older text, then the echo with a third writer's change folded in)
        // leave the shown text alone until the answer comes.
        let out = typed(&mut d, "!");
        let splice = out.iter().find_map(|o| match o {
            Out::Request { token, method, .. } if method == "splice" => Some(*token),
            _ => None,
        });
        d.call(Call::Doc { node: area, event: diff("well, héllo") });
        assert_eq!(prop(&d, area, "_value"), "well, héllo!", "an older text doesn't undo typing");
        d.call(Call::Doc { node: area, event: diff("Oh well, héllo!") });
        assert_eq!(prop(&d, area, "_value"), "well, héllo!", "held while the edit is in flight");
        d.call(Call::Reply { token: splice.unwrap(), result: Ok(json!({})) });
        assert_eq!(prop(&d, area, "_value"), "Oh well, héllo!", "then the newest text shows");
        assert_eq!(d.edits[&area].caret(), 15);
        // The field's own echo is no change at all.
        let out = d.call(Call::Doc { node: area, event: diff("Oh well, héllo!") });
        assert!(!out.iter().any(|o| matches!(o, Out::Part { .. })), "{out:?}");

        // When the field goes, so does its live query.
        let ops = json!([{"op": "destroy", "id": 2}]);
        let out = d.call(request(9, "commit", json!({"window": window, "ops": ops}), false));
        assert!(out.contains(&Out::Detach { node: area }), "{out:?}");
    }

    /// Apps that listen hear where the caret and selection are, and can select
    /// text and focus a field; Command-Up stays in a text area.
    #[test]
    fn apps_hear_and_move_the_caret_and_selection() {
        let mut d = desk();
        let w = open(
            &mut d,
            7,
            json!([
                {"op": "create", "id": 2, "type": "vstack", "props": {"padding": 8, "gap": 8}},
                {"op": "create", "id": 3, "type": "text_area", "props": {"value": "hello world", "w": 300, "h": 120}},
                {"op": "create", "id": 4, "type": "text_field", "props": {}},
                {"op": "insert", "parent": 1, "child": 2, "index": 0},
                {"op": "insert", "parent": 2, "child": 3, "index": 0},
                {"op": "insert", "parent": 2, "child": 4, "index": 1},
                {"op": "listen", "id": 3, "events": ["select"]}
            ]),
        );
        let (area, field) = (global(&d, w, 3), global(&d, w, 4));
        let commit = |d: &mut Desktop, ops: Value| {
            d.call(request(9, "commit", json!({"window": w, "ops": ops}), false))
        };
        let out = commit(&mut d, json!([{"op": "set", "id": 3, "props": {"undo_group": "x"}}]));
        assert!(matches!(out.last(), Some(Out::Respond { result: Err(_), .. })), "{out:?}");
        let selects = |out: &[Out]| -> Vec<Value> {
            events(out).into_iter().filter(|(_, e, _)| e == "select").map(|(_, _, v)| v).collect()
        };
        let out = commit(&mut d, json!([{"op": "set", "id": 3, "props": {"selection": [6, 11]}}]));
        assert_eq!(selects(&out), [json!([6, 11])]);
        assert_eq!(d.edits[&area].selected_text(), "world");
        assert_eq!(
            prop(&d, area, "_sel").as_array().map(Vec::len),
            Some(1),
            "highlighted, though not focused"
        );
        assert_eq!(prop(&d, area, "selection"), Value::Null, "a request, not a prop");
        // The same selection again is no news; a key that moves the caret is.
        assert!(
            selects(&commit(&mut d, json!([{"op": "set", "id": 3, "props": {"selection": [6, 11]}}])))
                .is_empty()
        );
        commit(&mut d, json!([{"op": "set", "id": 3, "props": {"focus": true}}]));
        assert_eq!(d.focus, Some(area));
        let out = d.input(Input::Key { key: Key::Up, mods: Mods { logo: true, ..Mods::default() } }).1;
        assert_eq!(selects(&out), [json!([0, 0])], "Command-Up: the text's start");
        assert!(
            d.apps.frame_of(w).is_some_and(|f| prop(&d, f, "w").as_f64() != Some(1280.0)),
            "not maximized"
        );
        // A field that doesn't listen says nothing; focus moves to it on request.
        commit(&mut d, json!([{"op": "set", "id": 4, "props": {"focus": true}}]));
        assert_eq!(d.focus, Some(field));
        assert!(selects(&typed(&mut d, "hi")).is_empty());
        let out = commit(&mut d, json!([{"op": "set", "id": 3, "props": {"selection": "all"}}]));
        assert!(matches!(out.last(), Some(Out::Respond { result: Err(_), .. })), "{out:?}");
    }

    /// Command-Z undoes a bound field's last run of typing through db, by its
    /// change sets; Command-Shift-Z undoes the undos.
    #[test]
    fn command_z_undoes_a_bound_fields_edits_through_db() {
        let mut d = desk();
        let w = open(
            &mut d,
            7,
            json!([
                {"op": "create", "id": 2, "type": "text_area", "props": {"bind": {"doc": "d1", "path": "body"}, "w": 300, "h": 120}},
                {"op": "insert", "parent": 1, "child": 2, "index": 0}
            ]),
        );
        let area = global(&d, w, 2);
        let requests = |out: &[Out], m: &str| -> Vec<(u64, Value)> {
            out.iter()
                .filter_map(|o| match o {
                    Out::Request { token, method, params, .. } if method == m => {
                        Some((*token, params.clone()))
                    }
                    _ => None,
                })
                .collect()
        };
        let at = center(&mut d, area);
        click(&mut d, at);
        let typing = requests(&typed(&mut d, "ab"), "splice");
        assert_eq!(typing.len(), 2);
        for (i, (token, _)) in typing.iter().enumerate() {
            d.call(Call::Reply { token: *token, result: Ok(json!({"change_set": 10 + i})) });
        }
        let cmd = |shift| Mods { logo: true, shift, ..Mods::default() };
        let undos = requests(&d.input(Input::Key { key: Key::Char('z'), mods: cmd(false) }).1, "undo");
        assert_eq!(
            undos.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>(),
            [
                json!({"change_set": 11, "as": "org.example.test"}),
                json!({"change_set": 10, "as": "org.example.test"})
            ],
            "the run of typing, newest first, as the app"
        );
        for (i, (token, _)) in undos.iter().enumerate() {
            d.call(Call::Reply { token: *token, result: Ok(json!({"change_set": 20 + i})) });
        }
        assert!(
            requests(&d.input(Input::Key { key: Key::Char('z'), mods: cmd(false) }).1, "undo").is_empty()
        );
        // The app's own change sets (a replace, say) undo as a step too.
        let ops = json!([{"op": "set", "id": 2, "props": {"undo_group": [30, 31]}}]);
        d.call(request(9, "commit", json!({"window": w, "ops": ops}), false));
        let undos = requests(&d.input(Input::Key { key: Key::Char('z'), mods: cmd(false) }).1, "undo");
        let sets: Vec<Value> = undos.iter().map(|(_, p)| p["change_set"].clone()).collect();
        assert_eq!(sets, [json!(31), json!(30)]);
        // Redo waits for db to answer the undo.
        assert!(requests(&d.input(Input::Key { key: Key::Char('Z'), mods: cmd(true) }).1, "undo").is_empty());
        for (i, (token, _)) in undos.iter().enumerate() {
            d.call(Call::Reply { token: *token, result: Ok(json!({"change_set": 40 + i})) });
        }
        let redos = requests(&d.input(Input::Key { key: Key::Char('Z'), mods: cmd(true) }).1, "undo");
        let sets: Vec<Value> = redos.iter().map(|(_, p)| p["change_set"].clone()).collect();
        assert_eq!(sets, [json!(41), json!(40)], "redo undoes the undos");
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
    fn secret_fields_show_bullets_and_keep_off_the_clipboard() {
        let mut d = desk();
        let w = open(
            &mut d,
            7,
            json!([
                {"op": "create", "id": 2, "type": "vstack", "props": {"padding": 10}},
                {"op": "create", "id": 3, "type": "text_field", "props": {"secret": true}},
                {"op": "insert", "parent": 1, "child": 2, "index": 0},
                {"op": "insert", "parent": 2, "child": 3, "index": 0}
            ]),
        );
        let field = global(&d, w, 3);
        let at = center(&mut d, field);
        click(&mut d, at);
        let out = typed(&mut d, "hunter2");
        assert!(
            out.iter().any(|o| matches!(o, Out::Part { data, .. } if data["value"] == "hunter2")),
            "the app hears the text"
        );
        let list = d.draw_list();
        let shown: Vec<_> = list
            .layers
            .iter()
            .flat_map(|l| &l.texts)
            .filter(|t| t.node == field)
            .map(|t| t.text.clone())
            .collect();
        assert_eq!(shown, ["•••••••"]);
        d.clipboard.text = "before".into();
        let cmd = Mods { logo: true, ..Mods::default() };
        d.input(Input::Key { key: Key::Char('a'), mods: cmd });
        d.input(Input::Key { key: Key::Char('c'), mods: cmd });
        d.input(Input::Key { key: Key::Char('x'), mods: cmd });
        assert_eq!(d.clipboard.text, "before");
        assert_eq!(prop(&d, field, "_value"), "hunter2", "cut did nothing");
    }

    #[test]
    fn a_dialog_takes_the_focus_when_it_opens() {
        let mut d = desk();
        let w = open(
            &mut d,
            7,
            json!([
                {"op": "create", "id": 2, "type": "vstack", "props": {"padding": 10}},
                {"op": "create", "id": 3, "type": "button", "props": {"label": "Join"}},
                {"op": "create", "id": 4, "type": "dialog", "props": {"hidden": true}},
                {"op": "create", "id": 5, "type": "text_field", "props": {"secret": true}},
                {"op": "insert", "parent": 1, "child": 2, "index": 0},
                {"op": "insert", "parent": 2, "child": 3, "index": 0},
                {"op": "insert", "parent": 2, "child": 4, "index": 1},
                {"op": "insert", "parent": 4, "child": 5, "index": 0}
            ]),
        );
        assert_eq!(d.focus, None);
        let show = json!({"window": w, "ops": [{"op": "set", "id": 4, "props": {"hidden": false}}]});
        d.call(request(9, "commit", show, false));
        assert_eq!(d.focus, Some(global(&d, w, 5)), "typing goes to the password field");
        let out = typed(&mut d, "pw");
        assert!(out.iter().any(|o| matches!(o, Out::Part { data, .. } if data["value"] == "pw")));
    }

    #[test]
    fn apps_can_ask_about_the_screen() {
        let mut d = desk();
        let out = d.call(request(1, "screen", Value::Null, false));
        assert_eq!(
            out,
            [Out::Respond {
                id: 1,
                result: Ok(json!({"width": 1280, "height": 800, "workspaces": 4, "workspace": 1}))
            }]
        );
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

    #[test]
    fn styles_follow_the_theme_and_an_apps_own_sheet() {
        use crate::scene::Rgba;
        let hex = |h: &str| Rgba::parse(h).unwrap();
        let mut d = desk();
        let css = ":root { --accent: #ff0000 } label.big { color: var(--accent); font-size: 20px }";
        let out = d.call(request(7, "open_window", json!({"title": "Styled", "style": css}), true));
        let [Out::Part { data, .. }] = &out[..] else { panic!("{out:?}") };
        let WindowEvent::Opened { window, .. } = serde_json::from_value(data.clone()).unwrap() else {
            panic!()
        };
        let ops = json!([
            {"op": "create", "id": 2, "type": "label", "props": {"text": "Hi", "class": "big"}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0}
        ]);
        d.call(request(8, "commit", json!({"window": window, "ops": ops}), false));
        let label = global(&d, window, 2);
        let s = d.scene.style(label);
        assert_eq!((s.color, s.font.size), (hex("#ff0000"), 20.0));
        // The app's sheet stops at its content area: the frame keeps the system's.
        let frame = d.apps.frame_of(window).unwrap();
        assert_eq!(d.scene.style(frame).var("accent"), hex("#3b82f6"));
        // A mistake fails open_window, saying where it is.
        let out = d.call(request(
            9,
            "open_window",
            json!({"title": "x", "style": "label {\n  colour: #fff }"}),
            true,
        ));
        let [Out::Respond { result: Err(e), .. }] = &out[..] else { panic!("{out:?}") };
        assert!(e.message.contains("line 2, column 3"), "{}", e.message);
        // The dark theme restyles everything, the desktop included, fading over
        // THEME_FADE.
        let desktop = |d: &mut Desktop| d.draw_list().layers[0].quads[0].color;
        let t0 = Instant::now();
        d.tick(t0);
        assert_eq!(desktop(&mut d), hex("#dfe5ec"));
        d.call(Call::Theme { dark: true });
        assert_eq!(desktop(&mut d), hex("#dfe5ec"), "it starts from where it was");
        d.tick(t0 + crate::anim::THEME_FADE / 2);
        let mid = desktop(&mut d);
        assert!(mid != hex("#dfe5ec") && mid != hex("#0b1220"), "{mid:?}");
        d.tick(t0 + crate::anim::THEME_FADE);
        assert_eq!(desktop(&mut d), hex("#0b1220"));
        assert_eq!(d.scene.style(label).color, hex("#ff0000"), "the app's own colour stays");
    }

    #[test]
    fn the_control_under_the_pointer_shows_its_hover_style() {
        let mut d = desk();
        let ops = json!([
            {"op": "create", "id": 2, "type": "button", "props": {"label": "Go", "x": 20, "y": 20, "w": 80, "h": 32}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0}
        ]);
        let window = open(&mut d, 7, ops);
        let button = global(&d, window, 2);
        let at = center(&mut d, button);
        let plain = d.scene.style(button).background;
        d.input(Input::MotionTo { x: at.0, y: at.1 });
        assert_eq!(d.scene.node(button).unwrap().props.get("_hover"), Some(&json!(true)));
        let hovered = d.scene.style(button);
        assert_eq!(hovered.background, Some(hovered.var("control-hover")));
        assert_ne!(hovered.background, plain);
        d.input(Input::MotionTo { x: 5.0, y: 790.0 });
        assert_ne!(d.scene.node(button).unwrap().props.get("_hover"), Some(&json!(true)));
        assert_eq!(d.scene.style(button).background, plain);
    }

    /// The first quad node `node` draws, in `d`'s draw list now.
    fn quad_of(d: &mut Desktop, node: NodeId) -> crate::scene::QuadItem {
        d.relayout();
        let (rect, _) = d.scene.screen_rects()[&node];
        d.draw_list()
            .layers
            .iter()
            .flat_map(|l| l.quads.iter())
            .find(|q| q.rect == rect)
            .cloned()
            .unwrap_or_else(|| panic!("node {node} draws no quad at {rect:?}"))
    }

    #[test]
    fn a_declared_transition_moves_a_prop_to_its_new_value() {
        let mut d = desk();
        let ops = json!([
            {"op": "create", "id": 2, "type": "rect",
             "props": {"x": 0, "y": 0, "w": 40, "h": 20, "color": "#000000", "transition": {"x": 100, "color": [100, "linear"]}}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0}
        ]);
        let window = open(&mut d, 7, ops);
        let rect = global(&d, window, 2);
        let t0 = Instant::now();
        d.tick(t0);
        let x0 = quad_of(&mut d, rect).rect.x;
        let set = json!([{"op": "set", "id": 2, "props": {"x": 100, "color": "#ffffff"}}]);
        d.call(request(9, "commit", json!({"window": window, "ops": set}), false));
        assert_eq!(d.scene.node(rect).unwrap().props["x"], 100, "the prop holds where it ends up");
        assert_eq!(quad_of(&mut d, rect).rect.x, x0, "drawn where it was");
        d.tick(t0 + Duration::from_millis(50));
        let mid = quad_of(&mut d, rect);
        assert!(mid.rect.x > x0 + 10.0 && mid.rect.x < x0 + 100.0, "{:?}", mid.rect);
        assert!(mid.color.0[0] > 60 && mid.color.0[0] < 200, "halfway grey: {:?}", mid.color);
        assert!(d.tick(t0 + Duration::from_millis(60)), "still moving");
        assert!(d.next_wake(t0) <= t0 + Duration::from_millis(16), "frames come while it moves");
        d.tick(t0 + Duration::from_millis(100));
        let end = quad_of(&mut d, rect);
        assert_eq!((end.rect.x, end.color), (x0 + 100.0, crate::scene::Rgba::parse("#ffffff").unwrap()));
        assert!(!d.tick(t0 + MOVE + Duration::from_millis(10)), "and stops (once the window has risen too)");
    }

    #[test]
    fn hovering_fades_a_controls_colours_and_reduce_motion_skips_it() {
        let mut d = desk();
        let ops = json!([
            {"op": "create", "id": 2, "type": "button", "props": {"label": "Go", "x": 20, "y": 20, "w": 80, "h": 32}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0}
        ]);
        let window = open(&mut d, 7, ops);
        let button = global(&d, window, 2);
        let at = center(&mut d, button);
        let t0 = Instant::now();
        d.tick(t0);
        let plain = quad_of(&mut d, button).color;
        d.input(Input::MotionTo { x: at.0, y: at.1 });
        let hover = d.scene.style(button).var("control-hover");
        assert_eq!(quad_of(&mut d, button).color, plain, "the fade starts from the old colour");
        d.tick(t0 + crate::anim::STATE_FADE);
        assert_eq!(quad_of(&mut d, button).color, hover);
        // With reduce motion, it changes at once.
        d.call(Call::ReduceMotion { on: true });
        d.input(Input::MotionTo { x: 5.0, y: 790.0 });
        assert_eq!(quad_of(&mut d, button).color, plain);
        assert!(!d.scene.animating());
    }

    #[test]
    fn windows_slide_in_when_switching_workspaces() {
        let mut d = desk();
        let window = open(&mut d, 7, json!([]));
        let frame = d.apps.frame_of(window).unwrap();
        let t0 = Instant::now();
        d.tick(t0);
        d.tick(t0 + MOVE); // the new window has risen into place
        let x = d.scene.screen_rects()[&frame].0.x;
        d.switch_workspace(2);
        d.switch_workspace(1);
        d.relayout();
        assert_eq!(d.scene.screen_rects()[&frame].0.x, x - SLIDE, "coming back to 1, from the left");
        d.tick(t0 + MOVE + MOVE);
        d.relayout();
        assert_eq!(d.scene.screen_rects()[&frame].0.x, x);
    }

    #[test]
    fn a_dialogs_backdrop_fades_in() {
        let mut d = desk();
        let ops = json!([
            {"op": "create", "id": 2, "type": "dialog", "props": {"hidden": true, "w": 200, "h": 100}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0}
        ]);
        let window = open(&mut d, 7, ops);
        let t0 = Instant::now();
        d.tick(t0);
        d.call(request(
            9,
            "commit",
            json!({"window": window, "ops": [{"op": "set", "id": 2, "props": {"hidden": false}}]}),
            false,
        ));
        let dialog = global(&d, window, 2);
        let backdrop = |d: &mut Desktop| {
            let style = d.scene.style(dialog);
            let full = style.var("backdrop");
            let list = d.draw_list();
            let alpha = list
                .layers
                .iter()
                .flat_map(|l| l.quads.iter())
                .find(|q| q.color.0[..3] == full.0[..3])
                .map(|q| q.color.0[3]);
            (alpha, full.0[3])
        };
        let (start, full) = backdrop(&mut d);
        assert_eq!(start, Some(0), "invisible as it appears");
        d.tick(t0 + crate::anim::STATE_FADE);
        assert_eq!(backdrop(&mut d).0, Some(full));
    }

    #[test]
    fn style_sheet_edits_restyle_live_and_a_broken_one_keeps_the_last_good() {
        use crate::scene::Rgba;
        let hex = |h: &str| Rgba::parse(h).unwrap();
        let mut d = desk();
        let out = d.call(request(
            7,
            "open_window",
            json!({"title": "A", "style": "label { color: #111111 }"}),
            true,
        ));
        let [Out::Part { data, .. }] = &out[..] else { panic!("{out:?}") };
        let WindowEvent::Opened { window, .. } = serde_json::from_value(data.clone()).unwrap() else {
            panic!()
        };
        let ops = json!([
            {"op": "create", "id": 2, "type": "label", "props": {"text": "Hi"}},
            {"op": "create", "id": 3, "type": "button", "props": {"label": "Go"}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0},
            {"op": "insert", "parent": 1, "child": 3, "index": 1}
        ]);
        d.call(request(8, "commit", json!({"window": window, "ops": ops}), false));
        let (label, button) = (global(&d, window, 2), global(&d, window, 3));
        let sheets = |v: &[(&str, &str)]| Call::StyleSheets {
            sheets: v.iter().map(|(t, c)| (t.to_string(), c.to_string())).collect(),
        };
        // System edits come after the built-in sheet; the app's own rule still wins
        // for its label, and the app's edits come after that.
        d.call(sheets(&[("system", "button { background: #ff0000 } label { color: #00ff00 }")]));
        assert_eq!(d.scene.style(button).background, Some(hex("#ff0000")));
        assert_eq!(d.scene.style(label).color, hex("#111111"));
        d.call(sheets(&[
            ("system", "button { background: #ff0000 }"),
            ("app:org.example.test", "label { color: #222222 }"),
        ]));
        assert_eq!(d.scene.style(label).color, hex("#222222"));
        // A mistake isn't applied: the last good edits stay, for each target.
        d.call(sheets(&[("system", "button { background: red }"), ("app:org.example.test", "label {")]));
        assert_eq!(d.scene.style(button).background, Some(hex("#ff0000")));
        assert_eq!(d.scene.style(label).color, hex("#222222"));
        // Empty edits, or none, put things back.
        d.call(sheets(&[("system", "")]));
        assert_eq!(d.scene.style(button).background, Some(d.scene.style(button).var("control")));
        assert_eq!(d.scene.style(label).color, hex("#111111"));
        // The prompt keeps the built-in look whatever the edits say.
        d.call(sheets(&[("system", ":root { --text: #ff00ff }")]));
        d.call(prompt_call(9, perm()));
        assert!(d.scene.node(crate::prompt::PROMPT).is_some(), "the prompt is up");
        assert_eq!(d.scene.style(crate::prompt::PROMPT).var("text"), hex("#1f2937"));
        assert_eq!(d.scene.style(button).var("text"), hex("#ff00ff"));
    }

    #[test]
    fn the_compositor_checks_style_sheets_and_hands_out_its_own() {
        let mut d = desk();
        let out = d.call(request(1, "check_style", json!({"css": "label {\n  colour: #fff }"}), false));
        assert_eq!(
            out,
            [Out::Respond {
                id: 1,
                result: Ok(
                    json!({"error": {"line": 2, "col": 3, "message": "no property \"colour\" (styles set colours and fonts only)"}})
                )
            }]
        );
        assert_eq!(
            d.call(request(2, "check_style", json!({"css": "label { color: #fff }"}), false)),
            [Out::Respond { id: 2, result: Ok(json!({})) }]
        );
        let [Out::Respond { result: Ok(v), .. }] =
            &d.call(request(3, "system_style", Value::Null, false))[..]
        else {
            panic!()
        };
        assert!(v["css"].as_str().unwrap().contains("--accent"));
    }

    #[test]
    fn a_press_shows_at_once_and_letting_go_fades() {
        let mut d = desk();
        let ops = json!([
            {"op": "create", "id": 2, "type": "button", "props": {"label": "Go", "x": 20, "y": 20, "w": 80, "h": 32}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0},
            {"op": "listen", "id": 2, "events": ["click"]}
        ]);
        let window = open(&mut d, 7, ops);
        let button = global(&d, window, 2);
        let at = center(&mut d, button);
        let t0 = Instant::now();
        d.tick(t0);
        d.input(Input::MotionTo { x: at.0, y: at.1 });
        d.tick(t0 + crate::anim::STATE_FADE);
        let hover = quad_of(&mut d, button).color;
        d.input(Input::Button { pressed: true });
        let pressed = d.scene.style(button).var("control-pressed");
        assert_eq!(quad_of(&mut d, button).color, pressed, "no fade into a press");
        d.input(Input::Button { pressed: false });
        assert_eq!(quad_of(&mut d, button).color, pressed, "letting go starts from the pressed colour");
        d.tick(t0 + crate::anim::STATE_FADE * 2);
        assert_eq!(quad_of(&mut d, button).color, hover);
        // Toggles and sliders draw their tracks in --track, not the pressed colour.
        assert!(crate::style::SYSTEM.contains("toggle, slider { background: var(--track); }"));
    }

    #[test]
    fn app_style_edits_find_windows_by_the_brokers_word_or_a_hosts_on_a_mac() {
        use crate::scene::Rgba;
        let red = Rgba::parse("#ff0000").unwrap();
        let mut d = desk();
        let open_as = |d: &mut Desktop, id: u64, from: Option<Sender>, app: &str| {
            let params = json!({"title": "W", "app": app});
            let out = d.call(Call::Request { id, from, method: "open_window".into(), params, stream: true });
            let [Out::Part { data, .. }] = &out[..] else { panic!("{out:?}") };
            let WindowEvent::Opened { window, .. } = serde_json::from_value(data.clone()).unwrap() else {
                panic!()
            };
            d.apps.content_of(window).unwrap()
        };
        // An app run by hand on a Mac connects with no identity: its host's word counts.
        let mac = open_as(&mut d, 1, Some(Sender::Conn(9)), "org.idealos.gallery");
        // An app the broker knows can't claim to be another.
        let liar = open_as(&mut d, 2, app(), "org.idealos.gallery");
        d.call(Call::StyleSheets {
            sheets: vec![("app:org.idealos.gallery".into(), ":root { --accent: #ff0000 }".into())],
        });
        assert_eq!(d.scene.style(mac).var("accent"), red);
        assert_ne!(d.scene.style(liar).var("accent"), red);
    }

    #[test]
    fn checkboxes_toggles_sliders_and_open_lists_follow_the_pointer() {
        let mut d = desk();
        let w = open(&mut d, 7, widgets());
        let hovered = |d: &mut Desktop, n: NodeId| {
            let at = center(d, n);
            d.input(Input::MotionTo { x: at.0, y: at.1 });
            let s = d.scene.style(n);
            (s.background, s.clone())
        };
        let checkbox = global(&d, w, 3);
        let (bg, s) = hovered(&mut d, checkbox);
        assert_eq!(bg, Some(s.var("control-hover")), "checkbox");
        for id in [4, 5] {
            let node = global(&d, w, id);
            let (bg, s) = hovered(&mut d, node);
            assert_eq!(bg, Some(s.var("track-hover")), "toggle and slider: node {id}");
        }
        // Open the select: the item under the pointer is the highlighted one.
        let select = global(&d, w, 6);
        let at = center(&mut d, select);
        click(&mut d, at);
        let menu = d.scene.screen_rects()[&MENU].0;
        let row_y = |d: &Desktop| d.scene.node(MENU + 2).unwrap().props["y"].as_f64().unwrap() as f32;
        assert_eq!(row_y(&d), 4.0, "the chosen value, A, starts highlighted");
        d.input(Input::MotionTo { x: menu.x + 20.0, y: menu.y + 4.0 + 2.5 * ROW });
        assert_eq!(row_y(&d), 4.0 + 2.0 * ROW, "C, under the pointer");
        // Choosing goes by the highlight, so a click there chooses C.
        d.input(Input::Button { pressed: true });
        d.input(Input::Button { pressed: false });
        assert_eq!(d.scene.node(select).unwrap().props["value"], "C");
    }

    fn key_with(d: &mut Desktop, key: Key, mods: Mods) {
        d.input(Input::Key { key, mods });
    }

    /// A window with a scroll view of 40 rows; returns it and the scroll view.
    fn scrolling(d: &mut Desktop) -> (u64, NodeId) {
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
        let w = open(d, 7, Value::Array(ops));
        let scroll = d.scene.parent(global(d, w, 10)).unwrap();
        d.relayout();
        (w, scroll)
    }

    #[test]
    fn text_fields_take_emacs_and_mac_keys() {
        let mut d = desk();
        let w = open(&mut d, 7, widgets());
        let field = global(&d, w, 7);
        let at = center(&mut d, field);
        click(&mut d, at);
        for c in "hello world".chars() {
            key(&mut d, Key::Char(c));
        }
        let ctrl = Mods { ctrl: true, ..Mods::default() };
        let cmd = Mods { logo: true, ..Mods::default() };
        let alt = Mods { alt: true, ..Mods::default() };
        let caret = |d: &Desktop| d.edits[&field].caret();
        key_with(&mut d, Key::Char('a'), ctrl);
        assert_eq!(caret(&d), 0, "Ctrl-A: the line's start");
        key_with(&mut d, Key::Char('e'), ctrl);
        assert_eq!(caret(&d), 11, "Ctrl-E: its end");
        key_with(&mut d, Key::Char('b'), ctrl);
        key_with(&mut d, Key::Char('b'), ctrl);
        key_with(&mut d, Key::Char('f'), ctrl);
        assert_eq!(caret(&d), 10, "Ctrl-B, -B, -F");
        key_with(&mut d, Key::Char('d'), ctrl);
        assert_eq!(d.text_value(field), "hello worl", "Ctrl-D deletes forward");
        key_with(&mut d, Key::Left, cmd);
        assert_eq!(caret(&d), 0, "Command-Left");
        key_with(&mut d, Key::Right, alt);
        assert_eq!(caret(&d), 5, "Option-Right: a word");
        key_with(&mut d, Key::Char('k'), ctrl);
        assert_eq!(d.text_value(field), "hello", "Ctrl-K kills to the end");
        key_with(&mut d, Key::Char('a'), ctrl);
        key_with(&mut d, Key::Char('y'), ctrl);
        assert_eq!(d.text_value(field), " worlhello", "Ctrl-Y yanks it back");
        key_with(&mut d, Key::Backspace, alt);
        assert_eq!(d.text_value(field), " hello", "Option-Backspace deletes the word before the caret");
        // Command-A selects everything; Ctrl-C still copies.
        key_with(&mut d, Key::Char('a'), cmd);
        key_with(&mut d, Key::Char('c'), ctrl);
        assert_eq!(d.clipboard.text, " hello");
        key_with(&mut d, Key::Char('q'), ctrl);
        assert_eq!(d.text_value(field), " hello", "other Ctrl letters type nothing");
    }

    #[test]
    fn windows_resize_from_any_edge_with_the_matching_cursor() {
        let mut d = desk();
        let w = open(&mut d, 7, json!([]));
        let frame = d.apps.frame_of(w).unwrap();
        let f = d.scene.local_rect(frame).unwrap();
        let size = d.apps.size_of(w).unwrap();
        // The left edge: the cursor says so, and dragging moves the window too.
        let at = (f.x + 2.0, f.y + f.h / 2.0);
        d.input(Input::MotionTo { x: at.0, y: at.1 });
        assert_eq!(d.cursor_shape(), Shape::ResizeEw);
        d.input(Input::Button { pressed: true });
        d.input(Input::MotionTo { x: at.0 - 50.0, y: at.1 });
        d.input(Input::Button { pressed: false });
        assert_eq!(d.apps.size_of(w).unwrap().0, size.0 + 50);
        assert_eq!(d.scene.local_rect(frame).unwrap().x, f.x - 50.0, "the right edge stays put");
        // Corners and the other edges show their cursors.
        let f = d.scene.local_rect(frame).unwrap();
        let shape_at = |d: &mut Desktop, x: f32, y: f32| {
            d.input(Input::MotionTo { x, y });
            d.cursor_shape()
        };
        assert_eq!(shape_at(&mut d, f.x + f.w - 2.0, f.y + f.h - 2.0), Shape::ResizeNwse);
        assert_eq!(shape_at(&mut d, f.x + 2.0, f.y + f.h - 2.0), Shape::ResizeNesw);
        assert_eq!(shape_at(&mut d, f.x + f.w / 2.0, f.y + f.h - 2.0), Shape::ResizeNs);
        assert_eq!(shape_at(&mut d, f.x + f.w / 2.0, f.y + f.h / 2.0), Shape::Arrow);
    }

    #[test]
    fn title_bars_show_hover_pressed_and_which_window_is_active() {
        let mut d = desk();
        let a = open(&mut d, 7, json!([]));
        let b = open(&mut d, 8, json!([]));
        d.draw_list();
        let bar = |d: &Desktop, w: u64| d.apps.title_parts(w).unwrap()[0];
        let has =
            |d: &Desktop, n: NodeId, k: &str| d.scene.node(n).unwrap().props.get(k) == Some(&json!(true));
        assert!(has(&d, bar(&d, b), "_active") && !has(&d, bar(&d, a), "_active"), "the frontmost is active");
        let s = d.scene.style(bar(&d, b));
        assert_eq!(s.background, Some(s.var("titlebar-active")));
        assert_eq!(d.scene.style(bar(&d, a)).background, Some(s.var("titlebar")));
        // Hover the active one's title bar, then press it to drag.
        let f = d.scene.local_rect(d.apps.frame_of(b).unwrap()).unwrap();
        d.input(Input::MotionTo { x: f.x + 60.0, y: f.y + 18.0 });
        assert!(has(&d, bar(&d, b), "_hover"));
        assert_eq!(d.scene.style(bar(&d, b)).background, Some(s.var("titlebar-hover")));
        d.input(Input::Button { pressed: true });
        assert_eq!(d.scene.style(bar(&d, b)).background, Some(s.var("titlebar-pressed")));
        d.input(Input::Button { pressed: false });
        assert!(!has(&d, bar(&d, b), "pressed"));
    }

    #[test]
    fn holding_a_select_open_and_letting_go_on_an_item_chooses_it() {
        let mut d = desk();
        let w = open(&mut d, 7, widgets());
        let select = global(&d, w, 6);
        let at = center(&mut d, select);
        // A click opens the list and leaves it open.
        click(&mut d, at);
        assert!(d.menu.is_some());
        d.close_menu();
        // Press, drag to the third item and let go: chosen.
        d.input(Input::MotionTo { x: at.0, y: at.1 });
        d.input(Input::Button { pressed: true });
        let m = d.menu.as_ref().unwrap().rect;
        d.input(Input::MotionTo { x: m.x + 20.0, y: m.y + 4.0 + 2.5 * ROW });
        d.input(Input::Button { pressed: false });
        assert!(d.menu.is_none());
        assert_eq!(d.scene.node(select).unwrap().props["value"], "C");
    }

    #[test]
    fn scroll_thumbs_drag_and_show_hover_while_mouse_drags_on_content_dont_scroll() {
        let mut d = desk();
        let (_, scroll) = scrolling(&mut d);
        let (r, _) = d.scene.screen_rects()[&scroll];
        // A mouse drag on the content leaves it where it is.
        let mid = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        d.input(Input::MotionTo { x: mid.0, y: mid.1 });
        d.input(Input::Button { pressed: true });
        d.input(Input::MotionTo { x: mid.0, y: mid.1 - 100.0 });
        d.input(Input::Button { pressed: false });
        assert_eq!(prop(&d, scroll, "_scroll").as_f64().unwrap_or(0.0), 0.0);
        // Over the thumb: it shows hover; dragging it to the bottom scrolls to the end.
        let t = crate::draw::scroll_thumb(r, &d.scene.node(scroll).unwrap().props).unwrap();
        d.input(Input::MotionTo { x: t.x + 2.0, y: t.y + 5.0 });
        assert_eq!(prop(&d, scroll, "_thumb"), "hover");
        d.input(Input::Button { pressed: true });
        assert_eq!(prop(&d, scroll, "_thumb"), "active");
        d.input(Input::MotionTo { x: t.x + 2.0, y: r.y + r.h + 50.0 });
        d.input(Input::Button { pressed: false });
        let extent = d.scroll_extent(scroll);
        assert_eq!(prop(&d, scroll, "_scroll").as_f64().unwrap() as f32, extent);
    }

    #[test]
    fn a_split_divider_shows_hover_and_a_resize_cursor() {
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
        assert_eq!(d.cursor_shape(), Shape::ResizeEw);
        let s = d.scene.style(split);
        assert_eq!(s.border, Some(s.var("divider-hover")));
        d.input(Input::MotionTo { x: r.x + 20.0, y: r.y + 50.0 });
        assert_eq!(d.cursor_shape(), Shape::Arrow);
        assert_eq!(d.scene.style(split).border, Some(s.var("control")));
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::*;
    use crate::demo;
    use crate::input::{Input, Key, Mods};
    use serde_json::json;

    fn desk() -> Desktop {
        let mut d = Desktop::with_fonts(1280.0, 800.0, crate::fonts::ui_only());
        d.scene.apply_all(demo::build(1280.0, 800.0, 0));
        d.set_screen(1280.0, 800.0);
        d
    }

    fn app() -> Option<Sender> {
        Some(Sender::App { app: "org.example.terminal".into(), conn: 3 })
    }

    fn call(d: &mut Desktop, id: u64, method: &str, params: Value, stream: bool) -> Vec<Out> {
        d.call(Call::Request { id, from: app(), method: method.into(), params, stream })
    }

    fn writes(out: &[Out]) -> Vec<Value> {
        out.iter()
            .filter_map(|o| match o {
                Out::Request { to, method, params, .. } if to == "pty" && method == "write" => {
                    Some(params.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_terminal_attaches_as_its_app_draws_frames_and_types() {
        let mut d = desk();
        let out =
            call(&mut d, 1, "open_window", json!({"title": "Terminal", "width": 500, "height": 320}), true);
        let [Out::Part { data, .. }] = &out[..] else { panic!("{out:?}") };
        let window = data["window"].as_u64().unwrap();
        let ops = json!([
            {"op": "create", "id": 3, "type": "vstack", "props": {"grow": 1}},
            {"op": "create", "id": 2, "type": "terminal", "props": {"pty": "7", "grow": 1}},
            {"op": "insert", "parent": 1, "child": 3, "index": 0},
            {"op": "insert", "parent": 3, "child": 2, "index": 0}
        ]);
        let out = call(&mut d, 2, "commit", json!({"window": window, "ops": ops}), false);
        let node = out
            .iter()
            .find_map(|o| match o {
                Out::Attach { node, params } => {
                    assert_eq!(params, &json!({"id": "7", "as": "org.example.terminal"}));
                    Some(*node)
                }
                _ => None,
            })
            .expect("attaches");
        // Laying out the commit tells ptyd the size that fits, as the app.
        let resize = out.iter().find_map(|o| match o {
            Out::Request { method, params, .. } if method == "resize" => Some(params.clone()),
            _ => None,
        });
        let resize = resize.expect("a resize");
        assert_eq!(resize["as"], "org.example.terminal");
        assert!(resize["cols"].as_u64().unwrap() > 20 && resize["rows"].as_u64().unwrap() > 5, "{resize}");

        // A frame becomes the node's lines; the cursor shows.
        let frame = json!({"cols": 40, "rows": 3, "lines": [[0, [{"text": "$ ls"}]], [1, [{"text": "a", "fg": "#cd3131"}]]], "cursor": [0, 4]});
        d.call(Call::Screen { node, frame });
        let p = &d.scene.node(node).unwrap().props;
        assert_eq!(p["_lines"][0][0]["text"], "$ ls");
        assert_eq!(p["_cursor"], json!([0, 4]));
        let list = d.draw_list();
        let texts: Vec<_> = list.layers.iter().flat_map(|l| &l.texts).filter(|t| t.node == node).collect();
        assert!(
            texts.iter().all(|t| t.font.families.iter().any(|f| f == "monospace")),
            "drawn in the monospace family"
        );
        assert!(texts.iter().any(|t| t.text == "$ ls") && texts.iter().any(|t| t.text == "a"));

        // Clicking focuses it; keys go to the shell (Tab and Escape too), as bytes.
        let (r, _) = d.scene.screen_rects()[&node];
        d.input(Input::MotionTo { x: r.x + 20.0, y: r.y + 20.0 });
        d.input(Input::Button { pressed: true });
        d.input(Input::Button { pressed: false });
        let key = |d: &mut Desktop, key, mods| d.input(Input::Key { key, mods }).1;
        let mut sent = Vec::new();
        for (k, m) in [
            (Key::Char('l'), Mods::default()),
            (Key::Tab, Mods::default()),
            (Key::Char('c'), Mods { ctrl: true, ..Mods::default() }),
            (Key::Escape, Mods::default()),
        ] {
            sent.extend(writes(&key(&mut d, k, m)));
        }
        let data: Vec<&str> = sent.iter().map(|w| w["data"].as_str().unwrap()).collect();
        assert_eq!(data, ["l", "\t", "\x03", "\x1b"]);
        assert!(sent.iter().all(|w| w["as"] == "org.example.terminal" && w["id"] == "7"));
        // Pasting sends the clipboard.
        d.clipboard.text = "echo hi\n".into();
        let out = key(&mut d, Key::Char('V'), Mods { ctrl: true, shift: true, ..Mods::default() });
        assert_eq!(writes(&out)[0]["data"], "echo hi\r");

        // When the node goes, so does the attachment.
        let ops = json!([{"op": "destroy", "id": 2}]);
        let out = call(&mut d, 3, "commit", json!({"window": window, "ops": ops}), false);
        assert!(out.contains(&Out::Detach { node }), "{out:?}");
    }
}
