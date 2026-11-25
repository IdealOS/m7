//! The retained scene graph.
//!
//! Apps mutate it with [`SceneOp`]s; each frame the renderer asks for a flattened
//! [`DrawList`]. Nodes are positioned with `x`/`y`/`w`/`h` props in logical
//! pixels, except the children of a `vstack` or `hstack`, which the stack lays out
//! in a row or column (`gap`, `padding`), sizing each from its own `w`/`h` or its
//! content. That's enough for M4's apps; real layout (taffy) arrives in M5.

use std::collections::HashMap;

use protocol::{NodeId, NodeKind, Props, SceneOp};
use serde_json::Value;

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
    pub size: f32,
    pub color: Rgba,
    pub bold: bool,
    /// Centered in `rect` horizontally (button labels), rather than left-aligned.
    pub center: bool,
    /// In the monospace family (a terminal's cells), rather than the UI font.
    pub mono: bool,
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
        Scene { nodes, dirty: true, layout_dirty: true, layout: HashMap::new(), content: HashMap::new() }
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
        let mut list = DrawList::default();
        for &child in self.top_level() {
            let mut layer = Layer::default();
            self.walk(child, SCREEN, None, Rect::INFINITE, &mut |id, node, rect, clip| {
                // A layer's text draws over all its quads, so a dialog, which must
                // cover its window's text too, starts a layer of its own.
                if node.kind == NodeKind::Dialog {
                    list.layers.push(std::mem::take(&mut layer));
                }
                crate::draw::draw(id, node, rect, clip, &mut layer)
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

pub fn color(p: &Props, key: &str, default: Rgba) -> Rgba {
    p.get(key).and_then(Value::as_str).and_then(parse_color).unwrap_or(default)
}

/// A color prop: `#rrggbb`, `#rrggbbaa`, or a theme token such as `$accent`.
pub fn parse_color(s: &str) -> Option<Rgba> {
    match s.strip_prefix('$') {
        Some(name) => Rgba::parse(crate::theme::token(name)?),
        None => Rgba::parse(s),
    }
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
