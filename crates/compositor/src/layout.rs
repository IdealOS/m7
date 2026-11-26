//! Layout with taffy: flexbox and grid for the nodes apps build their windows from.
//!
//! Containers (`vstack`, `hstack`, `toolbar`, `grid`, `split`, `scroll`, `dialog`)
//! place their children; everything else is placed by its `x`/`y` props as before.
//! A container whose parent isn't one is a layout root, sized by its own `w`/`h` or
//! else by its parent's rectangle (a window's content area), so an app's top stack
//! fills its window and reflows when the window is resized.
//!
//! Leaves are measured from their content: text by shaping it with the same fonts
//! and metrics the renderer uses (so wrapping matches what's drawn), controls by
//! their label plus fixed chrome. Results go into the scene as each node's
//! rectangle relative to its parent, which drawing and hit-testing both read.
//!
//! Props: `w`/`h` (a number of pixels, `"50%"`, or `"fill"` to take the free
//! space), `grow`, `min_w`/`min_h`/`max_w`/`max_h`, `padding` (one number, or
//! `[vertical, horizontal]`, or `[top, right, bottom, left]`), `gap`, `align`
//! (`start`, `center`, `end`, `stretch`) and `justify` (`start`, `center`, `end`,
//! `between`, `around`); `grid` takes `columns` (a count); `split` takes `ratio`
//! and `vertical`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use glyphon::{Buffer, FontSystem, Metrics, Shaping};
use protocol::{NodeId, NodeKind, Props};
use serde_json::Value;
use taffy::prelude::*;

use crate::paragraphs::Paragraphs;
use crate::scene::{Rect, Scene, num};
use crate::style::Font;

/// Width of a split view's divider.
pub const DIVIDER: f32 = 6.0;

/// What a taffy leaf stands for, for measuring.
#[derive(Clone, Copy)]
struct Leaf(NodeId);

/// A text's measured size, by (text, font, width limit).
type Measured = HashMap<(String, FontKey, u32), (f32, f32)>;

/// What a font is, for caching: families, size, weight, slant.
type FontKey = (std::sync::Arc<[String]>, u32, u16, bool);

fn font_key(f: &Font) -> FontKey {
    (f.families.clone(), f.size.to_bits(), f.weight, f.italic)
}

pub struct Layout {
    fonts: FontSystem,
    measured: Measured,
    /// Text widgets' paragraphs, for carets, selections and clicks.
    paragraphs: HashMap<NodeId, Paragraphs>,
}

/// What a layout pass found, for the scene to keep.
#[derive(Debug, Default)]
pub struct Computed {
    /// Each laid-out node's rectangle, relative to its parent.
    pub rects: HashMap<NodeId, Rect>,
    /// Each scroll view's content size, for scrolling limits.
    pub content: HashMap<NodeId, (f32, f32)>,
}

impl Layout {
    pub fn new(fonts: FontSystem) -> Layout {
        Layout { fonts, measured: HashMap::new(), paragraphs: HashMap::new() }
    }

    /// Lay out `scene` if anything layout depends on changed since last time.
    pub fn run(&mut self, scene: &mut Scene) {
        if !scene.needs_layout() {
            return;
        }
        let computed = self.compute(scene);
        scene.set_layout(computed);
        // Keep the cache from growing without bound as text changes.
        if self.measured.len() > 4096 {
            self.measured.clear();
        }
    }

    /// Lay out every layout root in `scene`.
    pub fn compute(&mut self, scene: &Scene) -> Computed {
        let mut out = Computed::default();
        let roots: Vec<NodeId> = scene
            .ids()
            .filter(|&id| {
                // Dialogs float over their parent, so each is a root even inside a
                // container (which leaves them out of its flow).
                let floats = scene.node(id).is_some_and(|n| n.kind == NodeKind::Dialog);
                is_container(scene, id)
                    && (floats || !scene.parent(id).is_some_and(|p| is_container(scene, p)))
                    && !hidden(scene, id)
            })
            .collect();
        for root in roots {
            self.lay_out_root(scene, root, &mut out);
        }
        out
    }

    fn lay_out_root(&mut self, scene: &Scene, root: NodeId, out: &mut Computed) {
        let mut tree: TaffyTree<Leaf> = TaffyTree::new();
        let mut ids: Vec<(taffy::NodeId, NodeId)> = Vec::new();
        let Some(top) = build(scene, &mut tree, root, &mut ids, true) else { return };
        let props = &scene.node(root).expect("root exists").props;
        // A root fills its parent (a window's content area) unless it has a size; a
        // dialog keeps its own size, since it floats over its parent.
        let floats = scene.node(root).is_some_and(|n| n.kind == NodeKind::Dialog);
        let parent = scene
            .parent(root)
            .and_then(|p| scene.local_rect(p))
            .filter(|r| r.w > 0.0 && r.h > 0.0 && !floats);
        let mut style = tree.style(top).expect("built").clone();
        if dim(props.get("w")).is_none()
            && let Some(p) = parent
        {
            style.size.width = Dimension::length(p.w);
        }
        if dim(props.get("h")).is_none()
            && let Some(p) = parent
        {
            style.size.height = Dimension::length(p.h);
        }
        tree.set_style(top, style).expect("built");
        let available = Size { width: AvailableSpace::MaxContent, height: AvailableSpace::MaxContent };
        let fonts = &mut self.fonts;
        let measured = &mut self.measured;
        let result = tree.compute_layout_with_measure(top, available, |inputs, _, leaf, style| {
            let leaf = leaf.copied();
            taffy::compute_leaf_layout(
                inputs,
                style,
                |_, _| 0.0,
                |known, available| {
                    let Some(Leaf(id)) = leaf else { return Size::ZERO };
                    let width = known.width.or(match available.width {
                        AvailableSpace::Definite(w) => Some(w),
                        AvailableSpace::MinContent => Some(1.0),
                        AvailableSpace::MaxContent => None,
                    });
                    let (w, h) = measure(fonts, measured, scene, id, width);
                    Size { width: known.width.unwrap_or(w), height: known.height.unwrap_or(h) }
                },
            )
        });
        if let Err(e) = result {
            log::warn!("layout: {e}");
            return;
        }
        for &(tid, id) in &ids {
            let l = tree.layout(tid).expect("computed");
            let rect = if id == root {
                // The root keeps its own position.
                let own = scene.local_rect(root).unwrap_or(Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 });
                Rect { x: own.x, y: own.y, w: l.size.width, h: l.size.height }
            } else {
                Rect { x: l.location.x, y: l.location.y, w: l.size.width, h: l.size.height }
            };
            out.rects.insert(id, rect);
        }
        // A scroll view's content extends to its lowest child, plus its padding.
        for (_, id) in
            ids.iter().copied().filter(|(_, id)| scene.node(*id).is_some_and(|n| n.kind == NodeKind::Scroll))
        {
            let node = scene.node(id).expect("listed");
            let pad = padding(node.props.get("padding")).map_or(0.0, |p| p.0[2]);
            let (mut w, mut h) = (0.0_f32, 0.0_f32);
            for c in &node.children {
                if let Some(r) = out.rects.get(c) {
                    w = w.max(r.x + r.w);
                    h = h.max(r.y + r.h);
                }
            }
            out.content.insert(id, (w, h + pad));
        }
    }
}

/// Whether `id` lays out its children.
pub fn is_container(scene: &Scene, id: NodeId) -> bool {
    scene.node(id).is_some_and(|n| container_kind(n.kind))
}

pub fn container_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Vstack
            | NodeKind::Hstack
            | NodeKind::Toolbar
            | NodeKind::Grid
            | NodeKind::Split
            | NodeKind::Scroll
            | NodeKind::Dialog
    )
}

fn hidden(scene: &Scene, id: NodeId) -> bool {
    scene.node(id).and_then(|n| n.props.get("hidden")).and_then(Value::as_bool) == Some(true)
}

/// Add `id` (and, for a container, its children) to `tree`.
fn build(
    scene: &Scene,
    tree: &mut TaffyTree<Leaf>,
    id: NodeId,
    ids: &mut Vec<(taffy::NodeId, NodeId)>,
    root: bool,
) -> Option<taffy::NodeId> {
    let node = scene.node(id)?;
    if hidden(scene, id) {
        return None;
    }
    // A dialog floats over its parent: a root of its own, not part of the flow.
    if node.kind == NodeKind::Dialog && !root {
        return None;
    }
    let mut style = style_for(node.kind, &node.props);
    let tid = if container_kind(node.kind) {
        let mut children = Vec::new();
        for (i, &c) in node.children.iter().enumerate() {
            let Some(child) = build(scene, tree, c, ids, false) else { continue };
            if node.kind == NodeKind::Split {
                // The first pane takes `ratio` of the space; the second the rest.
                let ratio = num(&node.props, "ratio", 0.5).clamp(0.05, 0.95);
                let mut cs = tree.style(child).expect("built").clone();
                if i == 0 {
                    cs.flex_basis = Dimension::percent(ratio);
                    cs.flex_grow = 0.0;
                } else {
                    cs.flex_basis = Dimension::length(0.0);
                    cs.flex_grow = 1.0;
                }
                cs.flex_shrink = 1.0;
                tree.set_style(child, cs).expect("built");
            } else if let Some(cross) = cross_axis(node.kind) {
                // "fill" across a stack (a column's width, a row's height) stretches
                // the child across it, rather than growing it along the stack.
                let cp = &scene.node(c).expect("built").props;
                let main = if cross == "w" { "h" } else { "w" };
                if matches!(dim(cp.get(cross)), Some(Dim::Fill))
                    && !matches!(dim(cp.get(main)), Some(Dim::Fill))
                {
                    let mut cs = tree.style(child).expect("built").clone();
                    cs.flex_grow = num(cp, "grow", 0.0);
                    cs.flex_basis = Dimension::auto();
                    cs.align_self = Some(AlignItems::STRETCH);
                    tree.set_style(child, cs).expect("built");
                }
            }
            children.push(child);
        }
        if node.kind == NodeKind::Scroll {
            // Content may be taller than the view; that's what scrolling is for.
            style.overflow = taffy::Point { x: taffy::Overflow::Hidden, y: taffy::Overflow::Hidden };
        }
        tree.new_with_children(style, &children).ok()?
    } else {
        if dim(node.props.get("w")).is_some() && dim(node.props.get("h")).is_some() {
            // Fully sized: nothing to measure.
            tree.new_leaf(style).ok()?
        } else {
            if node.kind == NodeKind::Vlist && dim(node.props.get("h")).is_none() {
                style.flex_grow = 1.0;
                style.min_size.height = length(80.0);
            }
            style.flex_shrink = if matches!(node.kind, NodeKind::Label | NodeKind::Text) { 1.0 } else { 0.0 };
            tree.new_leaf_with_context(style, Leaf(id)).ok()?
        }
    };
    ids.push((tid, id));
    Some(tid)
}

/// The size prop across a stack's direction (`w` in a column, `h` in a row).
fn cross_axis(kind: NodeKind) -> Option<&'static str> {
    match kind {
        NodeKind::Vstack | NodeKind::Scroll | NodeKind::Dialog => Some("w"),
        NodeKind::Hstack | NodeKind::Toolbar => Some("h"),
        _ => None,
    }
}

/// A number, a `"N%"` string, or `"fill"`.
enum Dim {
    Px(f32),
    Percent(f32),
    Fill,
}

fn dim(v: Option<&Value>) -> Option<Dim> {
    match v? {
        Value::Number(n) => n.as_f64().map(|n| Dim::Px(n as f32)),
        Value::String(s) if s == "fill" => Some(Dim::Fill),
        Value::String(s) => s.strip_suffix('%')?.trim().parse::<f32>().ok().map(|p| Dim::Percent(p / 100.0)),
        _ => None,
    }
}

fn style_for(kind: NodeKind, p: &Props) -> Style {
    let mut s = Style { display: Display::Flex, ..Style::default() };
    match kind {
        NodeKind::Vstack | NodeKind::Scroll | NodeKind::Dialog => s.flex_direction = FlexDirection::Column,
        NodeKind::Hstack | NodeKind::Toolbar => {
            s.flex_direction = FlexDirection::Row;
            s.align_items = AlignItems::CENTER;
        }
        NodeKind::Split => {
            let vertical = p.get("vertical").and_then(Value::as_bool) == Some(true);
            s.flex_direction = if vertical { FlexDirection::Column } else { FlexDirection::Row };
        }
        NodeKind::Grid => {
            s.display = Display::Grid;
            let columns = num(p, "columns", 2.0).clamp(1.0, 64.0) as u16;
            s.grid_template_columns = vec![repeat(columns, vec![fr(1.0)])];
        }
        _ => {}
    }
    match kind {
        NodeKind::Toolbar => {
            s.padding = Rect4::uniform(8.0).into();
            s.gap = Size { width: length(8.0), height: length(8.0) };
        }
        NodeKind::Split => s.gap = Size { width: length(DIVIDER), height: length(DIVIDER) },
        NodeKind::Dialog => {
            s.padding = Rect4::uniform(20.0).into();
            s.gap = Size { width: length(12.0), height: length(12.0) };
            s.size.width = Dimension::length(340.0);
        }
        _ => {}
    }
    for (key, axis) in [("w", 0), ("h", 1)] {
        match dim(p.get(key)) {
            Some(Dim::Px(v)) => set_size(&mut s, axis, Dimension::length(v)),
            Some(Dim::Percent(v)) => set_size(&mut s, axis, Dimension::percent(v)),
            Some(Dim::Fill) => {
                s.flex_grow = 1.0;
                s.flex_basis = Dimension::length(0.0);
            }
            None => {}
        }
    }
    if let Some(g) = p.get("grow").and_then(Value::as_f64) {
        s.flex_grow = g as f32;
    }
    if let Some(v) = p.get("min_w").and_then(Value::as_f64) {
        s.min_size.width = length(v as f32);
    }
    if let Some(v) = p.get("min_h").and_then(Value::as_f64) {
        s.min_size.height = length(v as f32);
    }
    if let Some(v) = p.get("max_w").and_then(Value::as_f64) {
        s.max_size.width = length(v as f32);
    }
    if let Some(v) = p.get("max_h").and_then(Value::as_f64) {
        s.max_size.height = length(v as f32);
    }
    if let Some(pad) = padding(p.get("padding")) {
        s.padding = pad.into();
    }
    if let Some(g) = p.get("gap").and_then(Value::as_f64) {
        s.gap = Size { width: length(g as f32), height: length(g as f32) };
    }
    if let Some(a) = p.get("align").and_then(Value::as_str) {
        s.align_items = match a {
            "start" => AlignItems::FLEX_START,
            "center" => AlignItems::CENTER,
            "end" => AlignItems::FLEX_END,
            _ => AlignItems::STRETCH,
        };
    }
    if let Some(j) = p.get("justify").and_then(Value::as_str) {
        s.justify_content = match j {
            "center" => JustifyContent::CENTER,
            "end" => JustifyContent::FLEX_END,
            "between" => JustifyContent::SPACE_BETWEEN,
            "around" => JustifyContent::SPACE_AROUND,
            _ => JustifyContent::FLEX_START,
        };
    }
    s
}

fn set_size(s: &mut Style, axis: usize, d: Dimension) {
    if axis == 0 {
        s.size.width = d;
    } else {
        s.size.height = d;
    }
}

/// Padding as top, right, bottom, left.
struct Rect4([f32; 4]);

impl Rect4 {
    fn uniform(v: f32) -> Rect4 {
        Rect4([v; 4])
    }
}

impl From<Rect4> for taffy::Rect<LengthPercentage> {
    fn from(r: Rect4) -> Self {
        let [t, rt, b, l] = r.0;
        taffy::Rect { top: length(t), right: length(rt), bottom: length(b), left: length(l) }
    }
}

fn padding(v: Option<&Value>) -> Option<Rect4> {
    let n = |v: &Value| v.as_f64().map(|n| n as f32);
    Some(match v? {
        Value::Number(_) => Rect4::uniform(n(v?)?),
        Value::Array(a) => match a.len() {
            2 => {
                let (vert, horiz) = (n(&a[0])?, n(&a[1])?);
                Rect4([vert, horiz, vert, horiz])
            }
            4 => Rect4([n(&a[0])?, n(&a[1])?, n(&a[2])?, n(&a[3])?]),
            _ => return None,
        },
        _ => return None,
    })
}

/// A leaf's size from its content, `width` limiting text (None for no limit).
fn measure(
    fonts: &mut FontSystem,
    cache: &mut Measured,
    scene: &Scene,
    id: NodeId,
    width: Option<f32>,
) -> (f32, f32) {
    let Some(node) = scene.node(id) else { return (0.0, 0.0) };
    let p = &node.props;
    let font = crate::draw::text_font(&scene.style(id), p);
    let s = |key: &str| p.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut text = |t: &str, width: Option<f32>| text_size(fonts, cache, t, &font, width);
    let explicit = |key: &str| p.get(key).and_then(Value::as_f64).map(|v| v as f32);
    let (w, h) = match node.kind {
        NodeKind::Text | NodeKind::Label => text(&s("text"), width),
        NodeKind::Button => {
            let (tw, _) = text(&s("label"), None);
            (tw + 28.0, 32.0)
        }
        NodeKind::Checkbox | NodeKind::Toggle => {
            let control = if node.kind == NodeKind::Toggle { 34.0 } else { 18.0 };
            let (tw, th) = text(&s("label"), None);
            (control + if tw > 0.0 { 8.0 + tw } else { 0.0 }, th.max(22.0))
        }
        NodeKind::Slider => (160.0, 24.0),
        NodeKind::TextField => (200.0, 32.0),
        NodeKind::TextArea => (280.0, 120.0),
        NodeKind::Select => {
            let longest = p
                .get("options")
                .and_then(Value::as_array)
                .map(|o| o.iter().filter_map(Value::as_str).map(|t| text(t, None).0).fold(0.0, f32::max))
                .unwrap_or(0.0);
            (longest.max(60.0) + 40.0, 32.0)
        }
        NodeKind::Menu => (text(&s("label"), None).0.max(40.0) + 40.0, 32.0),
        NodeKind::Image => (num(p, "_iw", 64.0), num(p, "_ih", 64.0)),
        NodeKind::Vlist => (200.0, 160.0),
        NodeKind::Terminal => (480.0, 300.0),
        _ => (0.0, 0.0),
    };
    (explicit("w").unwrap_or(w), explicit("h").unwrap_or(h))
}

/// The size `text` takes in `font`, wrapped at `width` if given, with the
/// renderer's metrics (line height 1.3 × size).
pub fn text_size(
    fonts: &mut FontSystem,
    cache: &mut Measured,
    text: &str,
    font: &Font,
    width: Option<f32>,
) -> (f32, f32) {
    let key = (text.to_owned(), font_key(font), width.unwrap_or(-1.0).to_bits());
    if let Some(&found) = cache.get(&key) {
        return found;
    }
    let size = font.size;
    let line = size * 1.3;
    let mut buffer = Buffer::new(fonts, Metrics::new(size, line));
    buffer.set_size(width, None);
    let attrs = crate::fonts::attrs(fonts.db(), font);
    buffer.set_text(text, &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(fonts, false);
    let (mut w, mut lines) = (0.0_f32, 0);
    for run in buffer.layout_runs() {
        w = w.max(run.line_w);
        lines += 1;
    }
    let found = (w.ceil(), line.ceil() * lines.max(1) as f32);
    cache.insert(key, found);
    found
}

/// Where text sits, for carets, selections and clicks: shaped with the layout
/// fonts and the renderer's metrics, so it lines up with what's drawn.
impl Layout {
    /// A terminal cell's width and height in `font` (a monospace one): its
    /// advance, and the renderer's line height.
    pub fn cell(&mut self, font: &Font) -> (f32, f32) {
        let size = font.size;
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(size, size * 1.3));
        buffer.set_size(None, None);
        let attrs = crate::fonts::attrs(self.fonts.db(), font);
        buffer.set_text(&"M".repeat(20), &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        let w = buffer.layout_runs().map(|r| r.line_w).fold(0.0, f32::max) / 20.0;
        (if w > 0.0 { w } else { size * 0.6 }, (size * 1.3).ceil())
    }

    /// Text widget `node`'s paragraphs, holding `text` in `font` wrapped at
    /// `width`: kept from the last call when they can be, so only what changed
    /// is shaped again.
    fn paragraphs(
        &mut self,
        node: NodeId,
        text: &str,
        font: &Font,
        width: Option<f32>,
    ) -> (&mut FontSystem, &mut Paragraphs) {
        let fonts = &mut self.fonts;
        let p = match self.paragraphs.entry(node) {
            Entry::Occupied(e) if e.get().fits(font, width, None) => {
                let p = e.into_mut();
                p.set_text(fonts, text);
                p
            }
            Entry::Occupied(mut e) => {
                e.insert(Paragraphs::new(fonts, text, font, width, None));
                e.into_mut()
            }
            Entry::Vacant(e) => e.insert(Paragraphs::new(fonts, text, font, width, None)),
        };
        (fonts, p)
    }

    /// Forget the paragraphs of text widgets `gone` says no longer exist.
    pub fn forget(&mut self, gone: impl Fn(NodeId) -> bool) {
        self.paragraphs.retain(|n, _| !gone(*n));
    }

    /// The caret for character `index` of `text` in text widget `node`: (x, y,
    /// height), from the text's top left. `width` wraps (a text area); `None`
    /// is one long line.
    pub fn caret_at(
        &mut self,
        node: NodeId,
        text: &str,
        font: &Font,
        width: Option<f32>,
        index: usize,
    ) -> (f32, f32, f32) {
        let (line, offset) = line_and_byte(text, index);
        let (fonts, p) = self.paragraphs(node, text, font, width);
        let (top, rows) = p.rows(fonts, line, line);
        let mut last = (0.0, top, (font.size * 1.3).ceil());
        for run in rows {
            let end = run.glyphs.last().map_or(0, |g| g.end);
            for g in run.glyphs {
                if offset <= g.start {
                    return (g.x, top + run.line_top, run.line_height);
                }
                if offset < g.end {
                    // Inside a glyph cluster: its trailing edge.
                    return (g.x + g.w, top + run.line_top, run.line_height);
                }
            }
            last = (run.glyphs.last().map_or(0.0, |g| g.x + g.w), top + run.line_top, run.line_height);
            if offset < end {
                return last;
            }
        }
        last
    }

    /// The character index nearest (`x`, `y`) in text widget `node`, measured
    /// from the text's top left.
    pub fn index_at(
        &mut self,
        node: NodeId,
        text: &str,
        font: &Font,
        width: Option<f32>,
        x: f32,
        y: f32,
    ) -> usize {
        let (fonts, p) = self.paragraphs(node, text, font, width);
        let Some(cursor) = p.hit(fonts, x.max(0.0), y.max(0.0)) else {
            return if y < 0.0 { 0 } else { text.chars().count() };
        };
        let mut chars = 0;
        for (i, line) in text.split('\n').enumerate() {
            if i == cursor.line {
                return chars + line[..cursor.index.min(line.len())].chars().count();
            }
            chars += line.chars().count() + 1;
        }
        text.chars().count()
    }

    /// The highlight rectangles for characters `a..b` of `text` in text widget
    /// `node`, one per row.
    #[allow(clippy::too_many_arguments)]
    pub fn selection_rects(
        &mut self,
        node: NodeId,
        text: &str,
        font: &Font,
        width: Option<f32>,
        a: usize,
        b: usize,
    ) -> Vec<Rect> {
        if a >= b {
            return Vec::new();
        }
        let (la, oa) = line_and_byte(text, a);
        let (lb, ob) = line_and_byte(text, b);
        let (fonts, p) = self.paragraphs(node, text, font, width);
        let (top, rows) = p.rows(fonts, la, lb);
        let mut out = Vec::new();
        for run in rows {
            let from = if run.line_i == la { oa } else { 0 };
            let to = if run.line_i == lb { ob } else { usize::MAX };
            let mut x0 = f32::MAX;
            let mut x1 = f32::MIN;
            for g in run.glyphs.iter().filter(|g| g.end > from && g.start < to) {
                x0 = x0.min(g.x);
                x1 = x1.max(g.x + g.w);
            }
            // A selected line break shows as a sliver at the line's end.
            if run.line_i < lb && run.glyphs.last().is_none_or(|g| g.end <= to) {
                let end = run.glyphs.last().map_or(0.0, |g| g.x + g.w);
                x0 = x0.min(end);
                x1 = x1.max(end + 4.0);
            }
            if x1 > x0 {
                out.push(Rect { x: x0, y: top + run.line_top, w: x1 - x0, h: run.line_height });
            }
        }
        out
    }

    /// The height of `text` in text widget `node`, wrapped at `width`: exact
    /// through `through` from its top, estimated below (see
    /// [`Paragraphs::height_through`]).
    pub fn text_height(
        &mut self,
        node: NodeId,
        text: &str,
        font: &Font,
        width: Option<f32>,
        through: f32,
    ) -> f32 {
        let (fonts, p) = self.paragraphs(node, text, font, width);
        p.height_through(fonts, through).ceil().max((font.size * 1.3).ceil())
    }
}

/// Character `index` of `text` as (line number, byte offset within that line).
fn line_and_byte(text: &str, index: usize) -> (usize, usize) {
    let mut left = index;
    for (i, line) in text.split('\n').enumerate() {
        let n = line.chars().count();
        if left <= n {
            let byte = line.char_indices().nth(left).map_or(line.len(), |(b, _)| b);
            return (i, byte);
        }
        left -= n + 1;
    }
    let lines = text.split('\n').count();
    (lines - 1, text.rsplit('\n').next().map_or(0, str::len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::ROOT;
    use protocol::SceneOp;
    use serde_json::json;

    fn op(v: Value) -> SceneOp {
        serde_json::from_value(v).unwrap()
    }

    /// A scene from ops, laid out with the bundled font.
    fn laid_out(ops: Value) -> Scene {
        let mut scene = Scene::new();
        for o in ops.as_array().unwrap() {
            scene.apply(op(o.clone()));
        }
        Layout::new(crate::fonts::ui_only()).run(&mut scene);
        scene
    }

    fn rect(scene: &Scene, id: NodeId) -> Rect {
        scene.layout_rect(id).unwrap_or_else(|| panic!("node {id} wasn't laid out"))
    }

    #[test]
    fn a_dialog_inside_a_container_is_laid_out_on_its_own() {
        // As the gallery has it: the dialog is the window's column's last child.
        let s = laid_out(json!([
            {"op": "create", "id": 1, "type": "group", "props": {"w": 500, "h": 400}},
            {"op": "create", "id": 2, "type": "vstack", "props": {}},
            {"op": "create", "id": 3, "type": "button", "props": {"label": "Open"}},
            {"op": "create", "id": 4, "type": "dialog", "props": {}},
            {"op": "create", "id": 5, "type": "label", "props": {"text": "A dialog"}},
            {"op": "insert", "parent": 0, "child": 1, "index": 0},
            {"op": "insert", "parent": 1, "child": 2, "index": 0},
            {"op": "insert", "parent": 2, "child": 3, "index": 0},
            {"op": "insert", "parent": 2, "child": 4, "index": 1},
            {"op": "insert", "parent": 4, "child": 5, "index": 0}
        ]));
        let d = rect(&s, 4);
        assert_eq!(d.w, 340.0, "its own width, not the column's");
        assert_eq!(d.h, 20.0 + (14.0f32 * 1.3).ceil() + 20.0, "its content and padding");
        assert_eq!(rect(&s, 5).w, 300.0, "its contents are laid out too");
        assert_eq!(rect(&s, 3).y, 0.0, "it takes no space in the column");
    }

    #[test]
    fn filling_across_a_stack_stretches_rather_than_grows() {
        // Settings' section buttons: a column of buttons as wide as the column.
        let s = laid_out(json!([
            {"op": "create", "id": 1, "type": "group", "props": {"w": 200, "h": 400}},
            {"op": "create", "id": 2, "type": "vstack", "props": {"h": 400, "w": 200, "gap": 4}},
            {"op": "create", "id": 3, "type": "button", "props": {"label": "Network", "w": "fill"}},
            {"op": "create", "id": 4, "type": "button", "props": {"label": "About", "w": "fill"}},
            {"op": "insert", "parent": 0, "child": 1, "index": 0},
            {"op": "insert", "parent": 1, "child": 2, "index": 0},
            {"op": "insert", "parent": 2, "child": 3, "index": 0},
            {"op": "insert", "parent": 2, "child": 4, "index": 1}
        ]));
        assert_eq!((rect(&s, 3).w, rect(&s, 3).h), (200.0, 32.0));
        assert_eq!(rect(&s, 4).y, 36.0);
    }

    #[test]
    fn text_sizes_are_kept_within_bounds() {
        // An app asking for enormous text gets the largest size, not a glyph the
        // size of a building.
        let s = laid_out(json!([
            {"op": "create", "id": 1, "type": "vstack", "props": {"w": 4000, "h": 4000}},
            {"op": "create", "id": 2, "type": "label", "props": {"text": "Hi", "size": 1e9}},
            {"op": "create", "id": 3, "type": "label", "props": {"text": "Hi", "size": -5}},
            {"op": "insert", "parent": 0, "child": 1, "index": 0},
            {"op": "insert", "parent": 1, "child": 2, "index": 0},
            {"op": "insert", "parent": 1, "child": 3, "index": 1}
        ]));
        assert_eq!(rect(&s, 2).h, (crate::theme::size::MAX * 1.3).ceil());
        assert_eq!(rect(&s, 3).h, (crate::theme::size::MIN * 1.3).ceil());
    }

    #[test]
    fn a_column_fills_its_parent_and_stacks_children_with_gap_and_padding() {
        let s = laid_out(json!([
            {"op": "create", "id": 1, "type": "group", "props": {"w": 300, "h": 200}},
            {"op": "create", "id": 2, "type": "vstack", "props": {"padding": 10, "gap": 6}},
            {"op": "create", "id": 3, "type": "label", "props": {"text": "Hello"}},
            {"op": "create", "id": 4, "type": "button", "props": {"label": "Go"}},
            {"op": "insert", "parent": 0, "child": 1, "index": 0},
            {"op": "insert", "parent": 1, "child": 2, "index": 0},
            {"op": "insert", "parent": 2, "child": 3, "index": 0},
            {"op": "insert", "parent": 2, "child": 4, "index": 1}
        ]));
        assert_eq!(rect(&s, 2), Rect { x: 0.0, y: 0.0, w: 300.0, h: 200.0 }, "fills the group");
        let label = rect(&s, 3);
        assert_eq!((label.x, label.y, label.w), (10.0, 10.0, 280.0), "stretched across, inside the padding");
        assert_eq!(label.h, (14.0f32 * 1.3).ceil(), "one line of text");
        let button = rect(&s, 4);
        assert_eq!((button.x, button.y, button.h), (10.0, 10.0 + label.h + 6.0, 32.0));
    }

    #[test]
    fn rows_center_and_fill_and_text_wraps_to_fit() {
        let s = laid_out(json!([
            {"op": "create", "id": 2, "type": "hstack", "props": {"w": 400, "h": 40, "gap": 10}},
            {"op": "create", "id": 3, "type": "button", "props": {"label": "A"}},
            {"op": "create", "id": 4, "type": "label", "props": {"text": "takes the rest", "w": "fill"}},
            {"op": "insert", "parent": 0, "child": 2, "index": 0},
            {"op": "insert", "parent": 2, "child": 3, "index": 0},
            {"op": "insert", "parent": 2, "child": 4, "index": 1}
        ]));
        let (a, rest) = (rect(&s, 3), rect(&s, 4));
        assert_eq!(a.y, 4.0, "centered: (40 - 32) / 2");
        assert_eq!(rest.x, a.w + 10.0);
        assert_eq!(rest.x + rest.w, 400.0, "fill takes the rest of the row");

        let narrow = laid_out(json!([
            {"op": "create", "id": 2, "type": "vstack", "props": {"w": 60}},
            {"op": "create", "id": 3, "type": "label", "props": {"text": "several words that wrap"}},
            {"op": "insert", "parent": 0, "child": 2, "index": 0},
            {"op": "insert", "parent": 2, "child": 3, "index": 0}
        ]));
        assert!(rect(&narrow, 3).h > 2.0 * 18.0, "wrapped onto more than two lines");
    }

    #[test]
    fn grids_splits_and_percentages() {
        let s = laid_out(json!([
            {"op": "create", "id": 2, "type": "grid", "props": {"w": 300, "columns": 3, "gap": 0}},
            {"op": "create", "id": 3, "type": "rect", "props": {"h": 20}},
            {"op": "create", "id": 4, "type": "rect", "props": {"h": 20}},
            {"op": "create", "id": 5, "type": "rect", "props": {"h": 20}},
            {"op": "create", "id": 6, "type": "rect", "props": {"h": 20}},
            {"op": "insert", "parent": 0, "child": 2, "index": 0},
            {"op": "insert", "parent": 2, "child": 3, "index": 0},
            {"op": "insert", "parent": 2, "child": 4, "index": 1},
            {"op": "insert", "parent": 2, "child": 5, "index": 2},
            {"op": "insert", "parent": 2, "child": 6, "index": 3},
            {"op": "create", "id": 10, "type": "split", "props": {"w": 406, "h": 100, "ratio": 0.25}},
            {"op": "create", "id": 11, "type": "vstack", "props": {}},
            {"op": "create", "id": 12, "type": "vstack", "props": {}},
            {"op": "insert", "parent": 0, "child": 10, "index": 1},
            {"op": "insert", "parent": 10, "child": 11, "index": 0},
            {"op": "insert", "parent": 10, "child": 12, "index": 1},
            {"op": "create", "id": 20, "type": "hstack", "props": {"w": 200, "h": 10}},
            {"op": "create", "id": 21, "type": "rect", "props": {"w": "25%", "h": 10}},
            {"op": "insert", "parent": 0, "child": 20, "index": 2},
            {"op": "insert", "parent": 20, "child": 21, "index": 0}
        ]));
        assert_eq!((rect(&s, 4).x, rect(&s, 4).w), (100.0, 100.0));
        assert_eq!((rect(&s, 6).x, rect(&s, 6).y), (0.0, 20.0), "the fourth wraps to row two");
        // The ratio is of the whole width, divider included (as CSS resolves it):
        // a quarter of 406, rounded.
        assert_eq!(rect(&s, 11).w, 102.0);
        assert_eq!(rect(&s, 12).x, 102.0 + DIVIDER);
        assert_eq!(rect(&s, 21).w, 50.0);
    }

    #[test]
    fn carets_selections_and_clicks_line_up() {
        let mut l = Layout::new(crate::fonts::ui_only());
        let (x0, y0, h) = l.caret_at(1, "hello", &Font::default(), None, 0);
        assert_eq!((x0, y0), (0.0, 0.0));
        assert!(h >= 18.0);
        let (x5, _, _) = l.caret_at(1, "hello", &Font::default(), None, 5);
        let (x2, _, _) = l.caret_at(1, "hello", &Font::default(), None, 2);
        assert!(0.0 < x2 && x2 < x5, "{x2} {x5}");
        assert_eq!(l.index_at(1, "hello", &Font::default(), None, x2 + 0.5, 5.0), 2);
        assert_eq!(l.index_at(1, "hello", &Font::default(), None, 500.0, 5.0), 5);
        // Two lines: the caret after the break is on the second line.
        let (_, y, _) = l.caret_at(1, "ab\ncd", &Font::default(), None, 3);
        assert!(y > 10.0);
        assert_eq!(l.index_at(1, "ab\ncd", &Font::default(), None, 0.0, y + 2.0), 3);
        let rects = l.selection_rects(1, "ab\ncd", &Font::default(), None, 1, 4);
        assert_eq!(rects.len(), 2, "{rects:?}");
        // Multi-byte characters count as one.
        let (xe, _, _) = l.caret_at(1, "éé", &Font::default(), None, 1);
        assert!(xe > 0.0 && xe < l.caret_at(1, "éé", &Font::default(), None, 2).0);
        assert_eq!(line_and_byte("ab\ncd", 4), (1, 1));
        assert_eq!(line_and_byte("é", 1), (0, 2));
    }

    #[test]
    fn scroll_views_record_their_content_and_hidden_nodes_take_no_space() {
        let mut ops = vec![
            json!({"op": "create", "id": 2, "type": "scroll", "props": {"w": 100, "h": 50, "gap": 0}}),
            json!({"op": "insert", "parent": 0, "child": 2, "index": 0}),
        ];
        for i in 0..10u64 {
            ops.push(
                json!({"op": "create", "id": 10 + i, "type": "rect", "props": {"h": 20, "hidden": i == 9}}),
            );
            ops.push(json!({"op": "insert", "parent": 2, "child": 10 + i, "index": i}));
        }
        let s = laid_out(Value::Array(ops));
        assert_eq!(rect(&s, 2).h, 50.0);
        assert_eq!(s.scroll_content(2).unwrap().1, 180.0, "nine visible rows of 20");
        assert_eq!(rect(&s, 18).y, 160.0);
        assert!(s.layout_rect(19).is_none(), "hidden");
        let _ = ROOT;
    }
}
