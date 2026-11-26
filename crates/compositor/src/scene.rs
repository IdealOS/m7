//! The retained scene graph.
//!
//! Apps mutate it with [`SceneOp`]s; each frame the renderer asks for a flattened
//! [`DrawList`]. Nodes are positioned with `x`/`y`/`w`/`h` props in logical
//! pixels, except the children of containers, which [`crate::layout`] places.
//!
//! Each node also has a computed style ([`crate::style`]): the system style sheet,
//! light or dark, and inside an app's windows the app's own sheet. Styles are
//! computed top-down when something asks for them after a change, and kept until
//! the next one; drawing, layout and text measuring all read the same ones.
//!
//! Positions and colours can move instead of jumping ([`crate::anim`]): a node's
//! `transition` prop names the props that move when set, a style's colours fade
//! when they change (a hover, a theme switch), and the desktop can move things too.
//! Props always hold where things end up; drawing and placement read where they
//! are now.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use protocol::{NodeId, NodeKind, Props, SceneOp};
use serde_json::Value;

use crate::anim::{self, Animator, Channel, Ease, StyleColor, Val};
use crate::style::{NodeInfo, Scope, Sheet, Style};

/// The implicit root every window is inserted under.
pub const ROOT: NodeId = 0;

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub props: Props,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
}

#[derive(Debug)]
pub struct Scene {
    nodes: HashMap<NodeId, Node>,
    /// Set by every mutation, cleared by the renderer. This is frame-level damage
    /// tracking: an idle desktop costs no GPU work. Region damage comes later.
    dirty: bool,
    /// Set by mutations that can change layout; cleared by a layout pass.
    layout_dirty: bool,
    /// Laid-out nodes' rectangles, relative to their parent (see [`crate::layout`]).
    layout: HashMap<NodeId, Rect>,
    /// Scroll views' content sizes.
    content: HashMap<NodeId, (f32, f32)>,
    styles: Styles,
    /// The styles last computed, to fade colours from when they change.
    shown_styles: RefCell<Option<Arc<HashMap<NodeId, Style>>>>,
    /// How long the next style change fades for.
    fade: Cell<std::time::Duration>,
    anim: RefCell<Animator>,
}

/// The style sheets in force, and the styles computed from them.
#[derive(Debug)]
struct Styles {
    /// The built-in system sheet ([`crate::style::SYSTEM`]).
    builtin: Arc<Sheet>,
    /// The system sheet in force: the built-in one with any edits after it.
    system: Arc<Sheet>,
    dark: bool,
    /// The system sheet alone: everything outside apps' windows.
    scope: Scope,
    /// The built-in sheet alone, for [`Scene::pin_builtin`] nodes.
    builtin_scope: Scope,
    /// Nodes (and what's in them) styled by the built-in sheet only.
    pinned: HashSet<NodeId>,
    /// Apps' sheets, by the node they style (a window's content area), with the
    /// scope each makes.
    apps: HashMap<NodeId, (Arc<Sheet>, Scope)>,
    /// Every node's computed style, until the next change.
    computed: RefCell<Option<Arc<HashMap<NodeId, Style>>>>,
}

impl Styles {
    fn new(builtin: Arc<Sheet>, system: Arc<Sheet>, dark: bool, pinned: HashSet<NodeId>) -> Styles {
        let scope = Scope::system(system.clone(), dark);
        let builtin_scope = Scope::system(builtin.clone(), dark);
        Styles {
            builtin,
            system,
            dark,
            scope,
            builtin_scope,
            pinned,
            apps: HashMap::new(),
            computed: RefCell::new(None),
        }
    }
}

/// Props that only change how a node looks, not the layout: setting just these
/// (as a window drag or a button press does) skips the next layout pass.
const VISUAL: &[&str] = &[
    "x",
    "y",
    "pressed",
    "color",
    "border_color",
    "border_width",
    "radius",
    "text_color",
    "_scroll",
    "_focus",
    "_caret",
    "_sel",
    "_scroll_x",
    "_hover",
    "_active",
    "_content",
    "_divider",
    "_range",
    "_lines",
    "_cursor",
    "_cell",
];

/// sRGB color with alpha, as written in props (`"#rrggbb"` or `"#rrggbbaa"`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba(pub [u8; 4]);

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba([0, 0, 0, 0]);
    pub const BLACK: Rgba = Rgba([0, 0, 0, 255]);

    pub fn parse(s: &str) -> Option<Rgba> {
        let hex = s.strip_prefix('#')?;
        let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
        match hex.len() {
            6 => Some(Rgba([byte(0)?, byte(2)?, byte(4)?, 255])),
            8 => Some(Rgba([byte(0)?, byte(2)?, byte(4)?, byte(6)?])),
            _ => None,
        }
    }

    /// Linear-light floats for the GPU (the render target is an sRGB format).
    pub fn to_linear(self) -> [f32; 4] {
        let c = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        [c(self.0[0]), c(self.0[1]), c(self.0[2]), self.0[3] as f32 / 255.0]
    }
}

/// Axis-aligned rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const INFINITE: Rect = Rect { x: -1e6, y: -1e6, w: 2e6, h: 2e6 };

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        Rect { x: x0, y: y0, w: (x1 - x0).max(0.0), h: (y1 - y0).max(0.0) }
    }

    /// The smallest rectangle holding both.
    pub fn union(&self, o: &Rect) -> Rect {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    pub fn area(&self) -> f32 {
        self.w.max(0.0) * self.h.max(0.0)
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// Grown by `by` on every side.
    pub fn expand(&self, by: f32) -> Rect {
        Rect { x: self.x - by, y: self.y - by, w: self.w + 2.0 * by, h: self.h + 2.0 * by }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuadItem {
    pub rect: Rect,
    pub color: Rgba,
    pub border_color: Rgba,
    pub border_width: f32,
    pub radius: f32,
    pub clip: Rect,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextItem {
    pub node: NodeId,
    /// Which of the node's texts this is (a checkbox draws its label and its check;
    /// a terminal, one per run of cells).
    pub slot: u32,
    pub text: String,
    /// Its family, size, weight and slant, from the node's style and props.
    pub font: crate::style::Font,
    pub color: Rgba,
    /// Centered in `rect` horizontally (button labels), rather than left-aligned.
    pub center: bool,
    pub rect: Rect,
    pub clip: Rect,
}

/// A blob image, drawn over the layer's rects and under its text.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageItem {
    pub node: NodeId,
    /// The blob's SHA-256.
    pub blob: String,
    /// The decoded pixels, once loaded (the desktop fills these in).
    pub pixels: Option<std::sync::Arc<crate::image::Image>>,
    pub rect: Rect,
    pub clip: Rect,
    pub radius: f32,
}

/// Everything under one child of the root draws as one layer: its rects, then its
/// text. Windows are root children, so a window on top covers the text of the
/// windows beneath it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Layer {
    /// Which layer this is from frame to frame: the root child it was drawn from, and
    /// which part of it (a dialog starts a new part). Damage tracking matches layers by
    /// it ([`crate::damage`]).
    pub key: (NodeId, u32),
    pub quads: Vec<QuadItem>,
    pub texts: Vec<TextItem>,
    pub images: Vec<ImageItem>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct DrawList {
    pub layers: Vec<Layer>,
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene {
    pub fn new() -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT,
            Node { kind: NodeKind::Group, props: Props::new(), children: Vec::new(), parent: None },
        );
        Scene {
            nodes,
            dirty: true,
            layout_dirty: true,
            layout: HashMap::new(),
            content: HashMap::new(),
            styles: {
                let builtin = Arc::new(Sheet::system());
                Styles::new(builtin.clone(), builtin, false, HashSet::new())
            },
            shown_styles: RefCell::new(None),
            fade: Cell::new(anim::STATE_FADE),
            anim: RefCell::new(Animator::default()),
        }
    }

    // --- Transitions ---------------------------------------------------------------

    /// Advance transitions to `now`: when the frame being drawn will reach the
    /// screen. Returns whether any still run. A frame is due while any do, and
    /// once more when the last ends.
    pub fn tick(&mut self, now: std::time::Instant) -> bool {
        let anim = self.anim.get_mut();
        let was = anim.is_animating();
        let running = anim.tick(now);
        if was {
            self.dirty = true;
        }
        running
    }

    /// Whether anything is moving.
    pub fn animating(&self) -> bool {
        self.anim.borrow().is_animating()
    }

    /// With reduce motion on, everything changes at once.
    pub fn set_reduce_motion(&mut self, on: bool) {
        let anim = self.anim.get_mut();
        anim.reduce_motion = on;
        if on {
            anim.clear();
        }
        self.dirty = true;
    }

    /// Move node `id`'s prop `prop` (one of [`anim::PROPS`]) from `from` to the
    /// value it has now.
    pub fn animate_from(
        &mut self,
        id: NodeId,
        prop: &'static str,
        from: Val,
        duration: std::time::Duration,
        ease: Ease,
    ) {
        let Some(to) = self.prop_value(id, prop, self.nodes.get(&id).and_then(|n| n.props.get(prop))) else {
            return;
        };
        self.anim.get_mut().start(id, Channel::Prop(prop), from, to, duration, ease);
        self.dirty = true;
    }

    /// A prop's value as a transition moves it: a number for `x` and `y`, a
    /// colour (in node `id`'s style) for the rest.
    fn prop_value(&self, id: NodeId, prop: &str, v: Option<&Value>) -> Option<Val> {
        let v = v?;
        match prop {
            "x" | "y" => v.as_f64().map(|n| Val::Num(n as f32)),
            _ => self.style(id).resolve(v.as_str()?).map(Val::Color),
        }
    }

    /// The transitions a `set` of `props` on `id` starts: each prop its
    /// `transition` names, from where it is now to its new value.
    fn transitions(&self, id: NodeId, props: &Props) -> Vec<(&'static str, Val, Val, anim::Spec)> {
        let Some(node) = self.nodes.get(&id) else { return Vec::new() };
        let mut out = Vec::new();
        for prop in anim::PROPS {
            let Some(new) = props.get(prop) else { continue };
            let spec = anim::spec(props, prop).or_else(|| anim::spec(&node.props, prop));
            let Some(spec) = spec else { continue };
            let shown = self.anim.borrow().value(id, Channel::Prop(prop));
            let (Some(from), Some(to)) = (
                shown.or_else(|| self.prop_value(id, prop, node.props.get(prop))),
                self.prop_value(id, prop, Some(new)),
            ) else {
                continue;
            };
            out.push((prop, from, to, spec));
        }
        out
    }

    /// Node `id` as it's drawn now: its moving props at their current values.
    fn moving_node(&self, id: NodeId, node: &Node) -> Node {
        let anim = self.anim.borrow();
        let mut node = node.clone();
        for prop in anim::PROPS {
            match anim.value(id, Channel::Prop(prop)) {
                Some(Val::Num(n)) => {
                    node.props.insert(prop.into(), n.into());
                }
                Some(Val::Color(c)) => {
                    node.props.insert(prop.into(), anim::hex(c).into());
                }
                None => {}
            }
        }
        if let Some(Val::Num(t)) = anim.value(id, Channel::Appear) {
            node.props.insert("_appear".into(), t.into());
        }
        node
    }

    /// Node `id`'s style as it's drawn now: its fading colours at their current values.
    fn moving_style(&self, id: NodeId, style: &Style) -> Style {
        let anim = self.anim.borrow();
        let mut style = style.clone();
        let at = |c: StyleColor| match anim.value(id, Channel::Style(c)) {
            Some(Val::Color(c)) => Some(c),
            _ => None,
        };
        if let Some(c) = at(StyleColor::Color) {
            style.color = c;
        }
        if let Some(c) = at(StyleColor::Accent) {
            style.accent = c;
        }
        if let Some(c) = at(StyleColor::Background) {
            style.background = Some(c);
        }
        if let Some(c) = at(StyleColor::Border) {
            style.border = Some(c);
        }
        style
    }

    /// Fade the colours that changed between each node's last style and its new
    /// one.
    fn fade_styles(&self, before: &HashMap<NodeId, Style>, after: &HashMap<NodeId, Style>) {
        let duration = self.fade.replace(anim::STATE_FADE);
        let mut anim = self.anim.borrow_mut();
        for (id, new) in after {
            let Some(old) = before.get(id) else { continue };
            let pairs = [
                (StyleColor::Color, Some(old.color), Some(new.color)),
                (StyleColor::Accent, Some(old.accent), Some(new.accent)),
                (StyleColor::Background, old.background, new.background),
                (StyleColor::Border, old.border, new.border),
            ];
            for (channel, from, to) in pairs {
                if let (Some(from), Some(to)) = (from, to)
                    && from != to
                {
                    anim.start(
                        *id,
                        Channel::Style(channel),
                        Val::Color(from),
                        Val::Color(to),
                        duration,
                        Ease::Out,
                    );
                }
            }
        }
    }

    // --- Styles --------------------------------------------------------------------

    /// Whether the dark theme is on.
    pub fn dark(&self) -> bool {
        self.styles.dark
    }

    /// Switch between the light and dark themes. Everything restyles at once.
    pub fn set_dark(&mut self, dark: bool) {
        if dark == self.styles.dark {
            return;
        }
        // Colour props that name a variable ($surface, ...) change with the theme
        // too: note them as they are, to fade from.
        let mut before = Vec::new();
        for (&id, node) in &self.nodes {
            for prop in ["color", "text_color", "border_color"] {
                let Some(text) = node.props.get(prop).and_then(Value::as_str) else { continue };
                if (text.starts_with('$') || text.starts_with("var("))
                    && let Some(c) = self.style(id).resolve(text)
                {
                    before.push((id, prop, c));
                }
            }
        }
        self.rebuild(self.styles.system.clone(), dark);
        self.fade.set(anim::THEME_FADE);
        for (id, prop, c) in before {
            self.animate_from(id, prop, Val::Color(c), anim::THEME_FADE, Ease::Out);
        }
    }

    /// Edits to the system style sheet (`None` for none): rules that come after the
    /// built-in sheet's. Things restyle at once, without fading, so editing a sheet
    /// live shows each change as it's typed.
    pub fn set_system_edits(&mut self, edits: Option<Arc<Sheet>>) {
        let system = match edits {
            Some(e) => Arc::new(self.styles.builtin.then(&e)),
            None => self.styles.builtin.clone(),
        };
        self.rebuild(system, self.styles.dark);
        self.fade.set(std::time::Duration::ZERO);
    }

    /// Style node `id` and everything in it with the built-in system sheet only,
    /// ignoring edits: the permission prompt, which must stay readable whatever a
    /// style does.
    pub fn pin_builtin(&mut self, id: NodeId) {
        self.styles.pinned.insert(id);
        self.restyled();
    }

    /// Replace the system sheet and the theme, keeping apps' sheets.
    fn rebuild(&mut self, system: Arc<Sheet>, dark: bool) {
        let apps: Vec<(NodeId, Arc<Sheet>)> =
            self.styles.apps.iter().map(|(id, (s, _))| (*id, s.clone())).collect();
        let pinned = std::mem::take(&mut self.styles.pinned);
        self.styles = Styles::new(self.styles.builtin.clone(), system, dark, pinned);
        for (id, sheet) in apps {
            self.set_app_style(id, Some(sheet));
        }
        self.restyled();
    }

    /// Style node `id` (a window's content area) and everything in it with an app's
    /// sheet as well as the system's, or (with `None`) the system's alone.
    pub fn set_app_style(&mut self, id: NodeId, sheet: Option<Arc<Sheet>>) {
        match sheet {
            Some(sheet) => {
                let scope = self.styles.scope.with_app(sheet.clone());
                self.styles.apps.insert(id, (sheet, scope));
            }
            None => {
                self.styles.apps.remove(&id);
            }
        }
        self.restyled();
    }

    fn restyled(&mut self) {
        self.styles.computed.replace(None);
        self.dirty = true;
        self.layout_dirty = true;
    }

    /// Every node's computed style (nodes in the tree under the root).
    pub fn styles(&self) -> Arc<HashMap<NodeId, Style>> {
        if let Some(s) = self.styles.computed.borrow().as_ref() {
            return s.clone();
        }
        let mut out = HashMap::with_capacity(self.nodes.len());
        let scope = &self.styles.scope;
        let root = scope.root(None);
        out.insert(ROOT, root.clone());
        let mut stack: Vec<(NodeId, Style, &Scope)> = Vec::new();
        for &c in self.nodes[&ROOT].children.iter().rev() {
            stack.push((c, root.clone(), scope));
        }
        while let Some((id, parent, scope)) = stack.pop() {
            let Some(node) = self.nodes.get(&id) else { continue };
            let (scope, parent) = match self.styles.apps.get(&id) {
                _ if self.styles.pinned.contains(&id) => {
                    (&self.styles.builtin_scope, self.styles.builtin_scope.root(None))
                }
                Some((_, app)) => (app, app.root(Some(&parent))),
                None => (scope, parent),
            };
            let style = scope.compute(&parent, &NodeInfo::of(node.kind, &node.props));
            for &c in node.children.iter().rev() {
                stack.push((c, style.clone(), scope));
            }
            out.insert(id, style);
        }
        let out = Arc::new(out);
        self.styles.computed.replace(Some(out.clone()));
        let before = self.shown_styles.replace(Some(out.clone()));
        if let Some(before) = before {
            self.fade_styles(&before, &out);
        }
        out
    }

    /// Node `id`'s computed style (the root's, for a node outside the tree).
    pub fn style(&self, id: NodeId) -> Style {
        let all = self.styles();
        all.get(&id).or_else(|| all.get(&ROOT)).cloned().expect("the root has a style")
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    /// Every node id, in no particular order.
    pub fn ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes.keys().copied()
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes.get(&id)?.parent
    }

    /// Whether something layout depends on changed since the last layout pass.
    pub fn needs_layout(&self) -> bool {
        self.layout_dirty
    }

    /// Keep a layout pass's results.
    pub fn set_layout(&mut self, computed: crate::layout::Computed) {
        self.layout = computed.rects;
        self.content = computed.content;
        self.layout_dirty = false;
        self.dirty = true;
    }

    /// Where the last layout pass put `id`, relative to its parent.
    pub fn layout_rect(&self, id: NodeId) -> Option<Rect> {
        self.layout.get(&id).copied()
    }

    /// A scroll view's content size, from the last layout pass.
    pub fn scroll_content(&self, id: NodeId) -> Option<(f32, f32)> {
        self.content.get(&id).copied()
    }

    /// Root child `root` and its visible descendants in paint order, with each one's
    /// rectangle on screen and its clip (from the last layout).
    pub fn paint_order(&self, root: NodeId) -> Vec<(NodeId, Rect, Rect)> {
        let mut out = Vec::new();
        self.walk(root, SCREEN, None, Rect::INFINITE, &mut |id, _, rect, clip| out.push((id, rect, clip)));
        out
    }

    /// Whether `id` is `ancestor` or inside it.
    pub fn is_within(&self, mut id: NodeId, ancestor: NodeId) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            match self.parent(id) {
                Some(p) => id = p,
                None => return false,
            }
        }
    }

    /// Every visible node's rectangle on screen and its clip, from the last layout.
    pub fn screen_rects(&self) -> HashMap<NodeId, (Rect, Rect)> {
        let mut out = HashMap::new();
        for &child in self.top_level() {
            self.walk(child, SCREEN, None, Rect::INFINITE, &mut |id, _, rect, clip| {
                out.insert(id, (rect, clip));
            });
        }
        out
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() == 1
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub fn apply_all(&mut self, ops: impl IntoIterator<Item = SceneOp>) {
        for op in ops {
            self.apply(op);
        }
    }

    /// Apply one op. Invalid ops (unknown ids, cycles) are logged and ignored:
    /// a buggy app must never be able to crash the compositor.
    pub fn apply(&mut self, op: SceneOp) {
        self.dirty = true;
        // Transitions start from the old values, so find them before the change.
        let starts = match &op {
            SceneOp::Set { id, props } => self.transitions(*id, props),
            _ => Vec::new(),
        };
        let appears = matches!(&op, SceneOp::Set { id, props }
            if props.get("hidden") == Some(&Value::Bool(false))
                && self.nodes.get(id).is_some_and(|n| n.kind == NodeKind::Dialog
                    && n.props.get("hidden").and_then(Value::as_bool) == Some(true)));
        if let SceneOp::Set { id, .. } = &op {
            let anim = self.anim.get_mut();
            for (prop, from, to, spec) in starts {
                anim.start(*id, Channel::Prop(prop), from, to, spec.duration, spec.ease);
            }
            if appears {
                anim.start(*id, Channel::Appear, Val::Num(0.0), Val::Num(1.0), anim::STATE_FADE, Ease::Out);
            }
        }
        self.styles.computed.replace(None);
        if !matches!(&op, SceneOp::Set { props, .. } if props.keys().all(|k| VISUAL.contains(&k.as_str()))) {
            self.layout_dirty = true;
        }
        match op {
            SceneOp::Create { id, kind, props } => {
                if id == ROOT {
                    log::warn!("create: node 0 is the root and cannot be replaced");
                    return;
                }
                if self.nodes.contains_key(&id) {
                    self.destroy(id);
                }
                self.nodes.insert(id, Node { kind, props, children: Vec::new(), parent: None });
            }
            SceneOp::Set { id, props } => match self.nodes.get_mut(&id) {
                Some(node) => node.props.extend(props),
                None => log::warn!("set: unknown node {id}"),
            },
            SceneOp::Insert { parent, child, index } => self.insert(parent, child, index),
            SceneOp::Remove { parent, child } => {
                if let Some(p) = self.nodes.get_mut(&parent) {
                    p.children.retain(|c| *c != child);
                }
                if let Some(c) = self.nodes.get_mut(&child) {
                    c.parent = None;
                }
            }
            SceneOp::Destroy { id } => self.destroy(id),
            SceneOp::Listen { .. } => {
                // Event routing back over the bus arrives with the bus in M2.
            }
        }
    }

    fn insert(&mut self, parent: NodeId, child: NodeId, index: usize) {
        if !self.nodes.contains_key(&parent) || !self.nodes.contains_key(&child) || child == ROOT {
            log::warn!("insert: bad ids parent={parent} child={child}");
            return;
        }
        // Refuse cycles: the child may not be an ancestor of the parent.
        let mut cursor = Some(parent);
        while let Some(id) = cursor {
            if id == child {
                log::warn!("insert: {child} is an ancestor of {parent}");
                return;
            }
            cursor = self.nodes[&id].parent;
        }
        if let Some(old) = self.nodes[&child].parent
            && let Some(p) = self.nodes.get_mut(&old)
        {
            p.children.retain(|c| *c != child);
        }
        let p = self.nodes.get_mut(&parent).expect("checked above");
        let index = index.min(p.children.len());
        p.children.insert(index, child);
        self.nodes.get_mut(&child).expect("checked above").parent = Some(parent);
    }

    fn destroy(&mut self, id: NodeId) {
        if id == ROOT {
            return;
        }
        let Some(node) = self.nodes.remove(&id) else { return };
        self.styles.apps.remove(&id);
        self.anim.get_mut().forget(id);
        if let Some(parent) = node.parent
            && let Some(p) = self.nodes.get_mut(&parent)
        {
            p.children.retain(|c| *c != id);
        }
        for child in node.children {
            if let Some(c) = self.nodes.get_mut(&child) {
                c.parent = None;
            }
            self.destroy(child);
        }
    }

    /// Move a root child to the top of the stack.
    pub fn raise(&mut self, id: NodeId) {
        let root = self.nodes.get_mut(&ROOT).expect("root always exists");
        if let Some(pos) = root.children.iter().position(|c| *c == id)
            && pos + 1 != root.children.len()
        {
            root.children.remove(pos);
            root.children.push(id);
            self.dirty = true;
        }
    }

    /// Root children, bottom to top.
    pub fn top_level(&self) -> &[NodeId] {
        &self.nodes[&ROOT].children
    }

    /// The node's rectangle from its own props (not offset by ancestors).
    pub fn local_rect(&self, id: NodeId) -> Option<Rect> {
        self.nodes.get(&id).map(|n| rect_of(&n.props))
    }

    pub fn draw_list(&self) -> DrawList {
        let styles = self.styles();
        let root = &styles[&ROOT];
        let mut list = DrawList::default();
        for &child in self.top_level() {
            let mut layer = Layer { key: (child, 0), ..Layer::default() };
            self.walk(child, SCREEN, None, Rect::INFINITE, &mut |id, node, rect, clip| {
                // A layer's text draws over all its quads, so a dialog, which must
                // cover its window's text too, starts a layer of its own.
                if node.kind == NodeKind::Dialog {
                    let next = Layer { key: (child, layer.key.1 + 1), ..Layer::default() };
                    list.layers.push(std::mem::replace(&mut layer, next));
                }
                let style = styles.get(&id).unwrap_or(root);
                if self.anim.borrow().moving(id) {
                    crate::draw::draw(id, node, rect, clip, &self.moving_style(id, style), &mut layer)
                } else {
                    crate::draw::draw(id, node, rect, clip, style, &mut layer)
                }
            });
            list.layers.push(layer);
        }
        list
    }

    /// The topmost node in `root` (a root child, such as a window) under (`x`, `y`)
    /// for which `wanted` is true, by where it's drawn (stack layout and clipping
    /// count). Pass the topmost window under the point, so covered nodes can't be hit.
    pub fn hit(&self, root: NodeId, x: f32, y: f32, wanted: impl Fn(NodeId) -> bool) -> Option<NodeId> {
        let mut found = None;
        self.walk(root, SCREEN, None, Rect::INFINITE, &mut |id, _, rect, clip| {
            if wanted(id) && rect.intersect(&clip).contains(x, y) {
                found = Some(id);
            }
        });
        found
    }

    /// Visit `id` and its descendants in paint order with each one's rectangle on
    /// screen and its clip. `parent` is the parent's rectangle on screen (shifted by
    /// its scroll offset, for a scroll view's children); `forced` places the node
    /// itself (a virtual list's rows).
    fn walk(
        &self,
        id: NodeId,
        parent: Rect,
        forced: Option<Rect>,
        clip: Rect,
        visit: &mut dyn FnMut(NodeId, &Node, Rect, Rect),
    ) {
        let Some(node) = self.nodes.get(&id) else { return };
        let moving;
        let node = if self.anim.borrow().moving(id) {
            moving = self.moving_node(id, node);
            &moving
        } else {
            node
        };
        let p = &node.props;
        if p.get("hidden").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let own = rect_of(p);
        let in_flow = node
            .parent
            .and_then(|p| self.nodes.get(&p))
            .is_some_and(|p| crate::layout::container_kind(p.kind));
        let mut local = match (forced, self.layout.get(&id)) {
            (Some(f), _) => f,
            // Laid out by its parent container.
            (None, Some(l)) if in_flow && node.kind != NodeKind::Dialog => *l,
            // A layout root: its own position, the computed size.
            (None, Some(l)) => Rect { x: own.x, y: own.y, w: l.w, h: l.h },
            (None, None) => own,
        };
        if node.kind == NodeKind::Dialog {
            // A dialog floats over its parent: centered across, a third of the way down.
            local.x = ((parent.w - local.w) / 2.0).max(0.0);
            local.y = ((parent.h - local.h) / 3.0).max(0.0);
        }
        let rect = Rect { x: parent.x + local.x, y: parent.y + local.y, ..local };
        visit(id, node, rect, clip);
        // Groups with `clip: true` clip their children; scrolling views always do.
        let clips = p.get("clip").and_then(Value::as_bool) == Some(true)
            || matches!(node.kind, NodeKind::Scroll | NodeKind::Vlist | NodeKind::Dialog);
        let child_clip = if clips { clip.intersect(&rect) } else { clip };
        let scrolled = num(p, "_scroll", 0.0);
        let inner = Rect { y: rect.y - scrolled, ..rect };
        let row_height = num(p, "row_height", 28.0).max(1.0);
        for &c in &node.children {
            let forced = (node.kind == NodeKind::Vlist).then(|| {
                let index = self.nodes.get(&c).map_or(0.0, |n| num(&n.props, "index", 0.0));
                Rect { x: 0.0, y: index * row_height, w: rect.w, h: row_height }
            });
            self.walk(c, inner, forced, child_clip, visit);
        }
    }
}

/// The screen as the parent of the root's children.
const SCREEN: Rect = Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };

pub fn num(p: &Props, key: &str, default: f32) -> f32 {
    p.get(key).and_then(Value::as_f64).map(|v| v as f32).unwrap_or(default)
}

/// A colour prop's value, or `default`: `#rrggbb`, `#rrggbbaa`, or a variable of
/// the node's style as `$accent` or `var(--accent)`.
pub fn color(p: &Props, key: &str, style: &Style, default: Rgba) -> Rgba {
    p.get(key).and_then(Value::as_str).and_then(|s| style.resolve(s)).unwrap_or(default)
}

fn rect_of(p: &Props) -> Rect {
    Rect { x: num(p, "x", 0.0), y: num(p, "y", 0.0), w: num(p, "w", 0.0), h: num(p, "h", 0.0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn props(v: Value) -> Props {
        v.as_object().unwrap().clone()
    }

    fn create(id: NodeId, kind: NodeKind, p: Value) -> SceneOp {
        SceneOp::Create { id, kind, props: props(p) }
    }

    fn insert(parent: NodeId, child: NodeId) -> SceneOp {
        SceneOp::Insert { parent, child, index: usize::MAX }
    }

    #[test]
    fn groups_offset_and_clip_their_children() {
        let mut s = Scene::new();
        s.apply_all([
            create(1, NodeKind::Group, json!({"x": 100, "y": 50, "w": 200, "h": 100, "clip": true})),
            create(2, NodeKind::Rect, json!({"x": 10, "y": 10, "w": 20, "h": 20, "color": "#ff0000"})),
            insert(ROOT, 1),
            insert(1, 2),
        ]);
        let list = s.draw_list();
        assert_eq!(list.layers.len(), 1);
        let q = &list.layers[0].quads[0];
        assert_eq!(q.rect, Rect { x: 110.0, y: 60.0, w: 20.0, h: 20.0 });
        assert_eq!(q.clip, Rect { x: 100.0, y: 50.0, w: 200.0, h: 100.0 });
        assert_eq!(q.color, Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn each_root_child_is_a_layer_in_stacking_order() {
        let mut s = Scene::new();
        s.apply_all([
            create(1, NodeKind::Rect, json!({})),
            create(2, NodeKind::Rect, json!({})),
            insert(ROOT, 1),
            insert(ROOT, 2),
        ]);
        assert_eq!(s.top_level(), &[1, 2]);
        s.raise(1);
        assert_eq!(s.top_level(), &[2, 1]);
        assert_eq!(s.draw_list().layers.len(), 2);
    }

    #[test]
    fn a_dialog_draws_over_its_windows_text() {
        let mut s = Scene::new();
        s.apply_all([
            create(1, NodeKind::Group, json!({"w": 300, "h": 200})),
            create(2, NodeKind::Text, json!({"w": 100, "h": 20, "text": "behind"})),
            create(3, NodeKind::Dialog, json!({"w": 200, "h": 100})),
            create(4, NodeKind::Text, json!({"w": 100, "h": 20, "text": "in front"})),
            insert(ROOT, 1),
            insert(1, 2),
            insert(1, 3),
            insert(3, 4),
        ]);
        let list = s.draw_list();
        let layer_of = |text: &str| list.layers.iter().position(|l| l.texts.iter().any(|t| t.text == text));
        let panel = list.layers.iter().position(|l| l.quads.iter().any(|q| q.rect.w == 200.0)).unwrap();
        assert!(layer_of("behind").unwrap() < panel, "the window's text is under the panel");
        assert_eq!(layer_of("in front"), Some(panel), "the dialog's own text is on it");
    }

    #[test]
    fn destroy_removes_descendants_and_detaches() {
        let mut s = Scene::new();
        s.apply_all([
            create(1, NodeKind::Group, json!({})),
            create(2, NodeKind::Group, json!({})),
            create(3, NodeKind::Text, json!({"text": "hi"})),
            insert(ROOT, 1),
            insert(1, 2),
            insert(2, 3),
            SceneOp::Destroy { id: 1 },
        ]);
        assert!(s.is_empty());
        assert!(s.top_level().is_empty());
    }

    #[test]
    fn cycles_and_unknown_ids_are_ignored() {
        let mut s = Scene::new();
        s.apply_all([
            create(1, NodeKind::Group, json!({})),
            create(2, NodeKind::Group, json!({})),
            insert(ROOT, 1),
            insert(1, 2),
            insert(2, 1),  // would make a cycle
            insert(99, 1), // unknown parent
            SceneOp::Set { id: 42, props: Props::new() },
        ]);
        assert_eq!(s.node(1).unwrap().parent, Some(ROOT));
        assert_eq!(s.node(2).unwrap().parent, Some(1));
    }

    #[test]
    fn set_merges_props_and_marks_dirty() {
        let mut s = Scene::new();
        s.apply(create(1, NodeKind::Text, json!({"text": "a", "size": 12})));
        s.clear_dirty();
        s.apply(SceneOp::Set { id: 1, props: props(json!({"text": "b"})) });
        assert!(s.is_dirty());
        let n = s.node(1).unwrap();
        assert_eq!(n.props["text"], "b");
        assert_eq!(n.props["size"], 12);
    }

    #[test]
    fn hex_colors_parse() {
        assert_eq!(Rgba::parse("#0a0b0c"), Some(Rgba([10, 11, 12, 255])));
        assert_eq!(Rgba::parse("#0a0b0c80"), Some(Rgba([10, 11, 12, 128])));
        assert_eq!(Rgba::parse("0a0b0c"), None);
        assert_eq!(Rgba::parse("#zz0000"), None);
    }
}
