//! What each node draws, from its props and the rectangle layout gave it.
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
use crate::theme;

fn rgba(s: &str) -> Rgba {
    Rgba::parse(s).expect("theme colors are valid")
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
        slot: u8,
        text: String,
        size: f32,
        fill: Rgba,
        bold: bool,
        center: bool,
        rect: Rect,
        clip: Rect,
    ) {
        self.layer.texts.push(TextItem {
            node: self.id,
            slot,
            text,
            size,
            color: fill,
            bold,
            center,
            rect,
            clip,
        });
    }
}

/// One line of text at `size`, vertically centered in `rect`.
fn line_in(rect: Rect, size: f32) -> Rect {
    let line = (size * 1.3).ceil();
    Rect { x: rect.x, y: rect.y + ((rect.h - line) / 2.0).max(0.0), w: rect.w, h: line }
}

/// Append what node `id` draws at `rect` (on screen) within `clip`.
pub fn draw(id: NodeId, node: &Node, rect: Rect, clip: Rect, layer: &mut Layer) {
    let p = &node.props;
    let mut o = Out { id, clip, layer };
    let size = theme::size::clamp(num(p, "size", theme::size::BODY));
    let focused = flag(p, "_focus");
    let border = rgba(theme::color::CONTROL_BORDER);
    let accent = rgba(theme::color::ACCENT);
    match node.kind {
        NodeKind::Rect => o.quad(
            rect,
            color(p, "color", Rgba::TRANSPARENT),
            color(p, "border_color", Rgba::TRANSPARENT),
            num(p, "border_width", 0.0),
            num(p, "radius", 0.0),
        ),
        NodeKind::Text | NodeKind::Label => {
            let center = p.get("align").and_then(Value::as_str) == Some("center");
            let bold = p.get("weight").and_then(Value::as_str) == Some("bold");
            o.text(
                0,
                text_of(p, "text"),
                size,
                color(p, "color", rgba(theme::color::TEXT)),
                bold,
                center,
                rect,
                clip,
            );
        }
        NodeKind::Button => {
            let fill = if flag(p, "pressed") { theme::color::CONTROL_PRESSED } else { theme::color::CONTROL };
            o.quad(rect, color(p, "color", rgba(fill)), border, 1.0, num(p, "radius", theme::radius::M));
            let label_clip = clip.intersect(&rect);
            let fill = color(p, "text_color", rgba(theme::color::TEXT));
            o.text(0, text_of(p, "label"), size, fill, false, true, line_in(rect, size), label_clip);
        }
        NodeKind::Checkbox | NodeKind::Toggle => {
            let on = flag(p, "checked");
            let line = line_in(rect, size);
            let label_x = if node.kind == NodeKind::Checkbox {
                let b = Rect { x: rect.x, y: rect.y + (rect.h - 18.0) / 2.0, w: 18.0, h: 18.0 };
                let fill = if on { accent } else { rgba(theme::color::SURFACE) };
                o.quad(b, fill, if on { accent } else { border }, 1.5, theme::radius::S);
                if on {
                    let check = Rect { x: b.x, y: line_in(b, 13.0).y, w: b.w, h: 18.0 };
                    o.text(1, "✓".into(), 13.0, rgba(theme::color::ACCENT_TEXT), true, true, check, clip);
                }
                b.x + 26.0
            } else {
                let track = Rect { x: rect.x, y: rect.y + (rect.h - 20.0) / 2.0, w: 34.0, h: 20.0 };
                o.quad(
                    track,
                    if on { accent } else { rgba(theme::color::CONTROL_PRESSED) },
                    Rgba::TRANSPARENT,
                    0.0,
                    10.0,
                );
                let knob_x = if on { track.x + 16.0 } else { track.x + 2.0 };
                let knob = Rect { x: knob_x, y: track.y + 2.0, w: 16.0, h: 16.0 };
                o.quad(knob, rgba(theme::color::SURFACE), Rgba::TRANSPARENT, 0.0, 8.0);
                track.x + 42.0
            };
            let label = Rect { x: label_x, w: (rect.x + rect.w - label_x).max(0.0), ..line };
            o.text(0, text_of(p, "label"), size, rgba(theme::color::TEXT), false, false, label, clip);
        }
        NodeKind::Slider => {
            let (min, max) = (num(p, "min", 0.0), num(p, "max", 100.0));
            let value = p.get("value").and_then(Value::as_f64).unwrap_or(min as f64) as f32;
            let t = if max > min { ((value - min) / (max - min)).clamp(0.0, 1.0) } else { 0.0 };
            let (x0, x1) = (rect.x + 8.0, rect.x + rect.w - 8.0);
            let cy = rect.y + rect.h / 2.0;
            let track = Rect { x: x0, y: cy - 2.0, w: (x1 - x0).max(0.0), h: 4.0 };
            o.quad(track, rgba(theme::color::CONTROL_PRESSED), Rgba::TRANSPARENT, 0.0, 2.0);
            o.quad(Rect { w: track.w * t, ..track }, accent, Rgba::TRANSPARENT, 0.0, 2.0);
            let knob = Rect { x: x0 + track.w * t - 8.0, y: cy - 8.0, w: 16.0, h: 16.0 };
            o.quad(knob, rgba(theme::color::SURFACE), if focused { accent } else { border }, 1.5, 8.0);
        }
        NodeKind::Select | NodeKind::Menu => {
            let fill = if flag(p, "pressed") { theme::color::CONTROL_PRESSED } else { theme::color::CONTROL };
            o.quad(rect, rgba(fill), border, 1.0, theme::radius::M);
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
            o.text(
                0,
                chosen.unwrap_or_default(),
                size,
                rgba(theme::color::TEXT),
                false,
                false,
                text_rect,
                inner,
            );
            let caret = Rect { x: rect.x + rect.w - 24.0, w: 16.0, ..line_in(rect, size) };
            o.text(1, "▾".into(), size, rgba(theme::color::MUTED), false, true, caret, inner);
        }
        NodeKind::TextField | NodeKind::TextArea => {
            let surface = rgba(theme::color::SURFACE);
            o.quad(
                rect,
                surface,
                if focused { accent } else { border },
                if focused { 2.0 } else { 1.0 },
                theme::radius::M,
            );
            let inner = Rect {
                x: rect.x + FIELD_PAD,
                y: rect.y + 4.0,
                w: (rect.w - 2.0 * FIELD_PAD).max(0.0),
                h: (rect.h - 8.0).max(0.0),
            };
            let inner_clip = clip.intersect(&inner);
            let value =
                p.get("_value").or(p.get("value")).and_then(Value::as_str).unwrap_or_default().to_owned();
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
                for r in sel {
                    let n = |i: usize| r.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32;
                    let r = Rect { x: origin.0 + n(0), y: origin.1 + n(1), w: n(2), h: n(3) };
                    o.quad_clipped(r, rgba(theme::color::SELECTION), Rgba::TRANSPARENT, 0.0, 0.0, inner_clip);
                }
            }
            let width = if multiline { inner.w } else { 0.0 };
            let text_rect =
                Rect { x: origin.0, y: origin.1, w: width, h: if multiline { inner.h + sy } else { line } };
            if value.is_empty() {
                let placeholder = text_of(p, "placeholder");
                o.text(1, placeholder, size, rgba(theme::color::MUTED), false, false, text_rect, inner_clip);
            } else {
                o.text(0, value, size, rgba(theme::color::TEXT), false, false, text_rect, inner_clip);
            }
            if focused && let Some(Value::Array(c)) = p.get("_caret") {
                let n = |i: usize| c.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32;
                let caret = Rect { x: origin.0 + n(0), y: origin.1 + n(1), w: 1.5, h: n(2).max(line) };
                o.quad_clipped(caret, rgba(theme::color::TEXT), Rgba::TRANSPARENT, 0.0, 0.0, inner_clip);
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
                _ => o.quad(rect, rgba(theme::color::SURFACE_ALT), border, 1.0, theme::radius::S),
            }
        }
        NodeKind::Toolbar => o.quad(rect, rgba(theme::color::SURFACE_ALT), Rgba::TRANSPARENT, 0.0, 0.0),
        NodeKind::Dialog => {
            // A backdrop over the rest of the window, then the panel.
            o.quad(clip, rgba(theme::color::BACKDROP), Rgba::TRANSPARENT, 0.0, 0.0);
            o.quad_clipped(
                rect,
                rgba(theme::color::SURFACE),
                border,
                1.0,
                theme::radius::L,
                Rect::INFINITE.intersect(&clip),
            );
        }
        NodeKind::Split => {
            // The divider sits between the two panes; layout left a gap for it.
            let vertical = flag(p, "vertical");
            let first = node.children.first().copied();
            let _ = first;
            let at = num(p, "_divider", -1.0);
            if at >= 0.0 {
                let d = if vertical {
                    Rect { x: rect.x, y: rect.y + at, w: rect.w, h: DIVIDER }
                } else {
                    Rect { x: rect.x + at, y: rect.y, w: DIVIDER, h: rect.h }
                };
                o.quad(d, rgba(theme::color::CONTROL), Rgba::TRANSPARENT, 0.0, 0.0);
            }
        }
        NodeKind::Scroll | NodeKind::Vlist => {
            // A thin scrollbar when the content is taller than the view.
            let content = num(p, "_content", 0.0);
            if content > rect.h + 0.5 {
                let scrolled = num(p, "_scroll", 0.0);
                let h = (rect.h * rect.h / content).max(24.0);
                let y = rect.y + (rect.h - h) * (scrolled / (content - rect.h)).clamp(0.0, 1.0);
                let bar = Rect { x: rect.x + rect.w - 6.0, y, w: 4.0, h };
                o.quad(bar, rgba("#64748b80"), Rgba::TRANSPARENT, 0.0, 2.0);
            }
        }
        _ => {}
    }
    // A focus ring around whatever has keyboard focus (text fields show theirs in
    // their border).
    if focused && !matches!(node.kind, NodeKind::TextField | NodeKind::TextArea | NodeKind::Slider) {
        let ring = Rect { x: rect.x - 3.0, y: rect.y - 3.0, w: rect.w + 6.0, h: rect.h + 6.0 };
        o.quad(ring, Rgba::TRANSPARENT, rgba(theme::color::FOCUS), 2.0, theme::radius::M + 3.0);
    }
}
