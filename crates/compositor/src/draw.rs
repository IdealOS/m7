//! What each node draws, from its props, its computed style ([`crate::style`]) and
//! the rectangle layout gave it. A colour prop the node sets (`color`,
//! `text_color`, ...) beats its style; otherwise colours and fonts come from the
//! style sheets, so a theme change restyles everything.
//!
//! Widgets keep their interactive state in props the compositor sets itself, all
//! starting with `_` (apps can't set those): `_focus` (keyboard focus), `_value`
//! (a text field's current text, or a slider's value while it's dragged), `_caret`
//! and `_sel` (caret and selection, measured by the widget code with the layout
//! fonts), `_scroll` and `_scroll_x` (scroll offsets). Drawing reads them and does
//! no measuring of its own.

use protocol::{NodeId, NodeKind, Props};
use serde_json::Value;

use crate::layout::DIVIDER;
use crate::scene::{ImageItem, Layer, Node, QuadItem, Rect, Rgba, TextItem, color, num};
use crate::style::{Font, Style};
use crate::theme;

/// The font a node's text is drawn and measured in: its style's, with the
/// node's own `size` and `weight: "bold"` props on top.
pub fn text_font(style: &Style, p: &Props) -> Font {
    let mut font = style.font.clone();
    if let Some(size) = p.get("size").and_then(Value::as_f64) {
        font.size = theme::size::clamp(size as f32);
    }
    if p.get("weight").and_then(Value::as_str) == Some("bold") {
        font.weight = font.weight.max(600);
    }
    font
}

fn flag(p: &Props, key: &str) -> bool {
    p.get(key).and_then(Value::as_bool) == Some(true)
}

fn text_of(p: &Props, key: &str) -> String {
    p.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// Padding inside text fields and areas.
pub const FIELD_PAD: f32 = 8.0;

struct Out<'a> {
    id: NodeId,
    clip: Rect,
    layer: &'a mut Layer,
}

impl Out<'_> {
    fn quad(&mut self, rect: Rect, fill: Rgba, border: Rgba, border_width: f32, radius: f32) {
        self.quad_clipped(rect, fill, border, border_width, radius, self.clip);
    }

    fn quad_clipped(
        &mut self,
        rect: Rect,
        fill: Rgba,
        border: Rgba,
        border_width: f32,
        radius: f32,
        clip: Rect,
    ) {
        self.layer.quads.push(QuadItem {
            rect,
            color: fill,
            border_color: border,
            border_width,
            radius,
            clip,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        slot: u32,
        text: String,
        font: &Font,
        fill: Rgba,
        center: bool,
        rect: Rect,
        clip: Rect,
    ) {
        self.layer.texts.push(TextItem {
            node: self.id,
            slot,
            text,
            font: font.clone(),
            color: fill,
            center,
            rect,
            clip,
        });
    }

    /// Unwrapped text at `rect`'s top left (a terminal's cells).
    fn cell_text(&mut self, slot: u32, text: String, font: Font, fill: Rgba, rect: Rect, clip: Rect) {
        let rect = Rect { w: 0.0, ..rect };
        self.layer.texts.push(TextItem {
            node: self.id,
            slot,
            text,
            font,
            color: fill,
            center: false,
            rect,
            clip,
        });
    }
}

/// Where scroll view props `p` at `rect` draw their thumb, if the content is taller
/// than the view: 4 pixels wide, 8 while hovered or dragged (`_thumb`).
pub fn scroll_thumb(rect: Rect, p: &Props) -> Option<Rect> {
    let content = num(p, "_content", 0.0);
    if content <= rect.h + 0.5 {
        return None;
    }
    let scrolled = num(p, "_scroll", 0.0);
    let h = (rect.h * rect.h / content).max(24.0);
    let y = rect.y + (rect.h - h) * (scrolled / (content - rect.h)).clamp(0.0, 1.0);
    let w = if p.contains_key("_thumb") && !p["_thumb"].is_null() { 8.0 } else { 4.0 };
    Some(Rect { x: rect.x + rect.w - w - 2.0, y, w, h })
}

/// One line of text at `size`, vertically centered in `rect`.
fn line_in(rect: Rect, size: f32) -> Rect {
    let line = (size * 1.3).ceil();
    Rect { x: rect.x, y: rect.y + ((rect.h - line) / 2.0).max(0.0), w: rect.w, h: line }
}

/// Append what node `id` draws at `rect` (on screen) within `clip`, in `style`.
pub fn draw(id: NodeId, node: &Node, rect: Rect, clip: Rect, style: &Style, layer: &mut Layer) {
    let p = &node.props;
    let mut o = Out { id, clip, layer };
    let font = text_font(style, p);
    let size = font.size;
    let focused = flag(p, "_focus");
    let accent = style.accent;
    let fill = style.background.unwrap_or(Rgba::TRANSPARENT);
    let border = style.border.unwrap_or(Rgba::TRANSPARENT);
    match node.kind {
        NodeKind::Rect => o.quad(
            rect,
            color(p, "color", style, fill),
            color(p, "border_color", style, border),
            num(p, "border_width", 0.0),
            num(p, "radius", 0.0),
        ),
        NodeKind::Text | NodeKind::Label => {
            let center = p.get("align").and_then(Value::as_str) == Some("center");
            if style.background.is_some() {
                o.quad(
                    rect,
                    fill,
                    border,
                    if style.border.is_some() { 1.0 } else { 0.0 },
                    num(p, "radius", 0.0),
                );
            }
            o.text(0, text_of(p, "text"), &font, color(p, "color", style, style.color), center, rect, clip);
        }
        NodeKind::Button => {
            o.quad(rect, color(p, "color", style, fill), border, 1.0, num(p, "radius", theme::radius::M));
            let label_clip = clip.intersect(&rect);
            let text = color(p, "text_color", style, style.color);
            o.text(0, text_of(p, "label"), &font, text, true, line_in(rect, size), label_clip);
        }
        NodeKind::Checkbox | NodeKind::Toggle => {
            let on = flag(p, "checked");
            let line = line_in(rect, size);
            let label_x = if node.kind == NodeKind::Checkbox {
                let b = Rect { x: rect.x, y: rect.y + (rect.h - 18.0) / 2.0, w: 18.0, h: 18.0 };
                o.quad(
                    b,
                    if on { accent } else { fill },
                    if on { accent } else { border },
                    1.5,
                    theme::radius::S,
                );
                if on {
                    let check = Rect { x: b.x, y: line_in(b, 13.0).y, w: b.w, h: 18.0 };
                    let mark = Font { size: 13.0, weight: 600, ..style.font.clone() };
                    o.text(1, "✓".into(), &mark, style.var("accent-text"), true, check, clip);
                }
                b.x + 26.0
            } else {
                let track = Rect { x: rect.x, y: rect.y + (rect.h - 20.0) / 2.0, w: 34.0, h: 20.0 };
                o.quad(track, if on { accent } else { fill }, Rgba::TRANSPARENT, 0.0, 10.0);
                let knob_x = if on { track.x + 16.0 } else { track.x + 2.0 };
                let knob = Rect { x: knob_x, y: track.y + 2.0, w: 16.0, h: 16.0 };
                o.quad(knob, style.var("surface"), Rgba::TRANSPARENT, 0.0, 8.0);
                track.x + 42.0
            };
            let label = Rect { x: label_x, w: (rect.x + rect.w - label_x).max(0.0), ..line };
            o.text(0, text_of(p, "label"), &font, style.color, false, label, clip);
        }
        NodeKind::Slider => {
            let (min, max) = (num(p, "min", 0.0), num(p, "max", 100.0));
            let value = p.get("value").and_then(Value::as_f64).unwrap_or(min as f64) as f32;
            let t = if max > min { ((value - min) / (max - min)).clamp(0.0, 1.0) } else { 0.0 };
            let (x0, x1) = (rect.x + 8.0, rect.x + rect.w - 8.0);
            let cy = rect.y + rect.h / 2.0;
            let track = Rect { x: x0, y: cy - 2.0, w: (x1 - x0).max(0.0), h: 4.0 };
            o.quad(track, fill, Rgba::TRANSPARENT, 0.0, 2.0);
            o.quad(Rect { w: track.w * t, ..track }, accent, Rgba::TRANSPARENT, 0.0, 2.0);
            let knob = Rect { x: x0 + track.w * t - 8.0, y: cy - 8.0, w: 16.0, h: 16.0 };
            let ring = if focused { accent } else { style.var("control-border") };
            o.quad(knob, style.var("surface"), ring, 1.5, 8.0);
        }
        NodeKind::Select | NodeKind::Menu => {
            o.quad(rect, fill, border, 1.0, theme::radius::M);
            let options: Vec<String> = p
                .get("options")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            // A select shows its value; a menu, its label.
            let chosen = if node.kind == NodeKind::Menu {
                Some(text_of(p, "label"))
            } else {
                p.get("value").and_then(Value::as_str).map(str::to_owned).or_else(|| options.first().cloned())
            };
            let inner = clip.intersect(&rect);
            let text_rect = Rect { x: rect.x + 10.0, w: (rect.w - 34.0).max(0.0), ..line_in(rect, size) };
            o.text(0, chosen.unwrap_or_default(), &font, style.color, false, text_rect, inner);
            let caret = Rect { x: rect.x + rect.w - 24.0, w: 16.0, ..line_in(rect, size) };
            o.text(1, "▾".into(), &font, style.var("muted"), true, caret, inner);
        }
        NodeKind::TextField | NodeKind::TextArea => {
            o.quad(rect, fill, border, if focused { 2.0 } else { 1.0 }, theme::radius::M);
            let inner = Rect {
                x: rect.x + FIELD_PAD,
                y: rect.y + 4.0,
                w: (rect.w - 2.0 * FIELD_PAD).max(0.0),
                h: (rect.h - 8.0).max(0.0),
            };
            let inner_clip = clip.intersect(&inner);
            let value =
                p.get("_value").or(p.get("value")).and_then(Value::as_str).unwrap_or_default().to_owned();
            // A password: one bullet per character (the desktop measures the same).
            let value = if flag(p, "secret") { "•".repeat(value.chars().count()) } else { value };
            let multiline = node.kind == NodeKind::TextArea;
            let (sx, sy) = (num(p, "_scroll_x", 0.0), num(p, "_scroll", 0.0));
            let line = (size * 1.3).ceil();
            // The text's origin: one line centered in a field, the top of an area.
            let origin = if multiline {
                (inner.x, inner.y + 2.0 - sy)
            } else {
                (inner.x - sx, rect.y + ((rect.h - line) / 2.0).max(0.0))
            };
            if let Some(Value::Array(sel)) = p.get("_sel") {
                let selection = style.selection.unwrap_or(style.var("selection"));
                for r in sel {
                    let n = |i: usize| r.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32;
                    let r = Rect { x: origin.0 + n(0), y: origin.1 + n(1), w: n(2), h: n(3) };
                    o.quad_clipped(r, selection, Rgba::TRANSPARENT, 0.0, 0.0, inner_clip);
                }
            }
            let width = if multiline { inner.w } else { 0.0 };
            let text_rect =
                Rect { x: origin.0, y: origin.1, w: width, h: if multiline { inner.h + sy } else { line } };
            if value.is_empty() {
                let placeholder = text_of(p, "placeholder");
                let muted = style.placeholder.unwrap_or(style.var("muted"));
                o.text(1, placeholder, &font, muted, false, text_rect, inner_clip);
            } else {
                o.text(0, value, &font, style.color, false, text_rect, inner_clip);
            }
            if focused && let Some(Value::Array(c)) = p.get("_caret") {
                let n = |i: usize| c.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32;
                let caret = Rect { x: origin.0 + n(0), y: origin.1 + n(1), w: 1.5, h: n(2).max(line) };
                let ink = style.caret.unwrap_or(style.color);
                o.quad_clipped(caret, ink, Rgba::TRANSPARENT, 0.0, 0.0, inner_clip);
            }
        }
        NodeKind::Terminal => {
            use crate::terminal::{PAD, SIZE};
            let font = Font { size: num(p, "size", SIZE), ..style.font.clone() };
            o.quad(rect, fill, if focused { accent } else { fill }, 1.0, theme::radius::S);
            let cursor = style.caret.unwrap_or(accent);
            let cell = p.get("_cell").and_then(Value::as_array);
            let n = |i: usize, d: f32| {
                cell.and_then(|c| c.get(i)).and_then(Value::as_f64).map_or(d, |v| v as f32)
            };
            let (cw, ch) = (n(0, 8.0), n(1, 18.0));
            let inner = clip.intersect(&rect);
            let at = |row: f32, col: f32, cols: f32| Rect {
                x: rect.x + PAD + col * cw,
                y: rect.y + PAD + row * ch,
                w: cols * cw,
                h: ch,
            };
            if let Some(Value::Array(c)) = p.get("_cursor") {
                let (r, col) = (
                    c.first().and_then(Value::as_f64).unwrap_or(0.0),
                    c.get(1).and_then(Value::as_f64).unwrap_or(0.0),
                );
                let block = at(r as f32, col as f32, 1.0);
                if focused {
                    o.quad_clipped(block, cursor, Rgba::TRANSPARENT, 0.0, 0.0, inner);
                } else {
                    o.quad_clipped(block, Rgba::TRANSPARENT, cursor, 1.0, 0.0, inner);
                }
            }
            if let Some(Value::Array(rows)) = p.get("_lines") {
                for (r, runs) in rows.iter().enumerate() {
                    let mut col = 0.0;
                    for (i, run) in runs.as_array().into_iter().flatten().enumerate() {
                        let text = run.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
                        let cols = text.chars().count() as f32;
                        let rect = at(r as f32, col, cols);
                        if let Some(bg) = run.get("bg").and_then(Value::as_str).and_then(Rgba::parse) {
                            o.quad_clipped(rect, bg, Rgba::TRANSPARENT, 0.0, 0.0, inner);
                        }
                        let fg = run
                            .get("fg")
                            .and_then(Value::as_str)
                            .and_then(Rgba::parse)
                            .unwrap_or(style.color);
                        if run.get("underline").and_then(Value::as_bool) == Some(true) {
                            o.quad_clipped(
                                Rect { y: rect.y + ch - 2.0, h: 1.0, ..rect },
                                fg,
                                Rgba::TRANSPARENT,
                                0.0,
                                0.0,
                                inner,
                            );
                        }
                        if !text.trim().is_empty() {
                            let bold = run.get("bold").and_then(Value::as_bool) == Some(true);
                            let f = Font { weight: if bold { 700 } else { font.weight }, ..font.clone() };
                            o.cell_text((r as u32) << 12 | i as u32, text, f, fg, rect, inner);
                        }
                        col += cols;
                    }
                }
            }
        }
        NodeKind::Surface => {
            // The service's latest frame, or a plain box until the first comes.
            match p.get("_frame").and_then(Value::as_str) {
                Some(frame) if num(p, "_iw", 0.0) > 0.0 => o.layer.images.push(ImageItem {
                    node: id,
                    blob: frame.to_owned(),
                    pixels: None,
                    rect,
                    clip,
                    radius: 0.0,
                }),
                _ => o.quad(rect, fill, Rgba::TRANSPARENT, 0.0, 0.0),
            }
        }
        NodeKind::Image => {
            match p.get("blob").and_then(Value::as_str) {
                Some(blob) if num(p, "_iw", 0.0) > 0.0 => {
                    let radius = num(p, "radius", 0.0);
                    o.layer.images.push(ImageItem {
                        node: id,
                        blob: blob.to_owned(),
                        pixels: None,
                        rect,
                        clip,
                        radius,
                    });
                }
                // Not loaded (yet): a placeholder box.
                _ => o.quad(rect, fill, border, 1.0, theme::radius::S),
            }
        }
        NodeKind::Dialog => {
            // A backdrop over the rest of the window (fading in as the dialog
            // appears, `_appear` going from 0 to 1), then the panel.
            let mut backdrop = style.var("backdrop");
            backdrop.0[3] = (backdrop.0[3] as f32 * num(p, "_appear", 1.0).clamp(0.0, 1.0)).round() as u8;
            o.quad(clip, backdrop, Rgba::TRANSPARENT, 0.0, 0.0);
            o.quad_clipped(rect, fill, border, 1.0, theme::radius::L, Rect::INFINITE.intersect(&clip));
        }
        NodeKind::Split => {
            // The divider sits between the two panes; layout left a gap for it.
            let vertical = flag(p, "vertical");
            let at = num(p, "_divider", -1.0);
            if at >= 0.0 {
                let d = if vertical {
                    Rect { x: rect.x, y: rect.y + at, w: rect.w, h: DIVIDER }
                } else {
                    Rect { x: rect.x + at, y: rect.y, w: DIVIDER, h: rect.h }
                };
                o.quad(d, border, Rgba::TRANSPARENT, 0.0, 0.0);
            }
        }
        NodeKind::Scroll | NodeKind::Vlist => {
            if style.background.is_some() {
                o.quad(
                    rect,
                    fill,
                    border,
                    if style.border.is_some() { 1.0 } else { 0.0 },
                    num(p, "radius", 0.0),
                );
            }
            // A thin scrollbar when the content is taller than the view, wider and
            // darker while hovered or dragged (`_thumb`).
            if let Some(bar) = scroll_thumb(rect, p) {
                let ink = match p.get("_thumb").and_then(Value::as_str) {
                    Some("active") => style.var("scrollbar-active"),
                    Some(_) => style.var("scrollbar-hover"),
                    None => style.var("scrollbar"),
                };
                o.quad(bar, ink, Rgba::TRANSPARENT, 0.0, bar.w / 2.0);
            }
        }
        // Containers draw only what a style gives them: `.card { background: ... }`.
        NodeKind::Group | NodeKind::Vstack | NodeKind::Hstack | NodeKind::Grid | NodeKind::Toolbar => {
            if style.background.is_some() || style.border.is_some() {
                o.quad(
                    rect,
                    fill,
                    border,
                    if style.border.is_some() { 1.0 } else { 0.0 },
                    num(p, "radius", 0.0),
                );
            }
        }
    }
    // A focus ring around whatever has keyboard focus (text fields show theirs in
    // their border).
    if focused
        && !matches!(
            node.kind,
            NodeKind::TextField
                | NodeKind::TextArea
                | NodeKind::Slider
                | NodeKind::Terminal
                | NodeKind::Surface
        )
    {
        let ring = Rect { x: rect.x - 3.0, y: rect.y - 3.0, w: rect.w + 6.0, h: rect.h + 6.0 };
        o.quad(ring, Rgba::TRANSPARENT, style.var("focus"), 2.0, theme::radius::M + 3.0);
    }
}
