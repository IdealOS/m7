//! Region damage: which parts of the screen changed between two frames, and what to
//! redraw in a buffer that last held an older frame.
//!
//! Damage comes from comparing each frame's [`DrawList`] with the last one. The draw
//! list is exactly what gets drawn, so anything that would draw differently (an app's
//! commit, a hover, a caret, the clock, a window moving) is damage by construction,
//! and nothing that changes the picture can be missed. Layers are matched by their key
//! (the window they were drawn from): a layer that appears, disappears or changes place
//! in the stacking order damages its whole area, old and new. Within a layer that kept
//! its place, items are compared one by one, and the boxes around the changed items,
//! before and after, are the damage: dragging a window gives where it was and where it
//! is.
//!
//! The KMS backend draws into three scanout buffers in turn, so the buffer being drawn
//! holds the frame from three frames ago. [`DamageTracker`] keeps the last few frames'
//! damage and gives each buffer everything that changed since it was last drawn.

use std::collections::{HashMap, VecDeque};

use protocol::NodeId;

use crate::scene::{DrawList, ImageItem, Layer, QuadItem, Rect, TextItem};

/// Room around an item for what's drawn just outside its rect: the quad shader's
/// one-pixel anti-aliasing pad (at scales up to 2), plus rounding to whole pixels.
const MARGIN: f32 = 2.0;
/// At most this many rectangles per redraw; more are merged.
const MAX_RECTS: usize = 4;
/// Damage covering more of the screen than this is redrawn whole: a full redraw clears
/// for free, where a partial one keeps the old contents and fills the damage.
const FULL_FRACTION: f32 = 0.6;
/// Frames of damage kept, for buffers that last held an older frame.
const HISTORY: usize = 8;

/// Damage between frames, per scanout buffer.
pub struct DamageTracker {
    prev: Option<(DrawList, (f32, f32))>,
    /// The number of the last frame recorded.
    frame: u64,
    /// Each recent frame's damage, oldest first; `None` was a full redraw.
    history: VecDeque<(u64, Option<Vec<Rect>>)>,
    /// For each buffer, the frame it holds, once drawn.
    drawn: Vec<Option<u64>>,
}

impl DamageTracker {
    pub fn new(buffers: usize) -> DamageTracker {
        DamageTracker { prev: None, frame: 0, history: VecDeque::new(), drawn: vec![None; buffers] }
    }

    /// Compare `list`, for a screen of `screen` (logical size), with the last frame's.
    /// Returns the rectangles that changed, `None` for everything (the first frame, a new
    /// screen size, or too much changed), or an empty list when nothing visible changed,
    /// in which case nothing is recorded and the frame can be skipped.
    pub fn next_frame(&mut self, list: &DrawList, screen: (f32, f32)) -> Option<Vec<Rect>> {
        let damage = match &self.prev {
            Some((prev, size)) if *size == screen => {
                diff(prev, list).and_then(|rects| simplify(rects, screen))
            }
            _ => None,
        };
        self.prev = Some((list.clone(), screen));
        if damage.as_ref().is_some_and(Vec::is_empty) {
            return damage;
        }
        self.frame += 1;
        self.history.push_back((self.frame, damage.clone()));
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
        damage
    }

    /// What to redraw in `buffer` for the frame [`DamageTracker::next_frame`] just
    /// recorded: everything that changed since the buffer was last drawn. `None` means all
    /// of it (the buffer was never drawn, or held a frame older than the history).
    pub fn region_for(&self, buffer: usize, screen: (f32, f32)) -> Option<Vec<Rect>> {
        let since = self.drawn.get(buffer).copied().flatten()?;
        if self.frame - since > self.history.len() as u64 {
            return None;
        }
        let mut rects = Vec::new();
        for (frame, damage) in &self.history {
            if *frame > since {
                rects.extend(damage.as_ref()?.iter().copied());
            }
        }
        simplify(rects, screen)
    }

    /// `buffer` now holds the frame just recorded.
    pub fn drew(&mut self, buffer: usize) {
        if let Some(d) = self.drawn.get_mut(buffer) {
            *d = Some(self.frame);
        }
    }
}

/// The rectangles that differ between two draw lists, or `None` if layers can't be
/// matched (two with the same key).
fn diff(old: &DrawList, new: &DrawList) -> Option<Vec<Rect>> {
    let index = |list: &DrawList| -> Option<HashMap<(NodeId, u32), usize>> {
        let mut map = HashMap::new();
        for (i, layer) in list.layers.iter().enumerate() {
            if map.insert(layer.key, i).is_some() {
                return None;
            }
        }
        Some(map)
    };
    let (was, now) = (index(old)?, index(new)?);
    let mut out = Vec::new();
    for (key, &n) in &now {
        match was.get(key) {
            None => out.extend(layer_bounds(&new.layers[n])),
            // Another place in the stacking order: what it covers, and is covered by,
            // changed everywhere it is and was.
            Some(&o) if o != n => {
                out.extend(layer_bounds(&old.layers[o]));
                out.extend(layer_bounds(&new.layers[n]));
            }
            Some(&o) => layer_diff(&old.layers[o], &new.layers[n], &mut out),
        }
    }
    for (key, &o) in &was {
        if !now.contains_key(key) {
            out.extend(layer_bounds(&old.layers[o]));
        }
    }
    Some(out)
}

/// Damage within a layer that kept its place: the box around its changed items
/// before, and the box around them after.
fn layer_diff(a: &Layer, b: &Layer, out: &mut Vec<Rect>) {
    if a.quads.len() != b.quads.len() || a.texts.len() != b.texts.len() || a.images.len() != b.images.len() {
        out.extend(layer_bounds(a));
        out.extend(layer_bounds(b));
        return;
    }
    let (mut before, mut after) = (None, None);
    let mut changed = |x: Rect, y: Rect| {
        grow(&mut before, x);
        grow(&mut after, y);
    };
    for (x, y) in a.quads.iter().zip(&b.quads) {
        if x != y {
            changed(quad_bounds(x), quad_bounds(y));
        }
    }
    for (x, y) in a.texts.iter().zip(&b.texts) {
        if x != y {
            changed(text_bounds(x), text_bounds(y));
        }
    }
    for (x, y) in a.images.iter().zip(&b.images) {
        if !same_image(x, y) {
            changed(image_bounds(x), image_bounds(y));
        }
    }
    out.extend(before);
    out.extend(after);
}

/// Images compare by what they show, without comparing their pixels: the blob names
/// them, and the pixels arrive once (loading counts as a change).
fn same_image(a: &ImageItem, b: &ImageItem) -> bool {
    a.blob == b.blob
        && a.rect == b.rect
        && a.clip == b.clip
        && a.radius == b.radius
        && a.pixels.is_some() == b.pixels.is_some()
}

fn grow(acc: &mut Option<Rect>, r: Rect) {
    if !r.is_empty() {
        *acc = Some(acc.map_or(r, |a| a.union(&r)));
    }
}

fn layer_bounds(l: &Layer) -> Option<Rect> {
    let mut acc = None;
    l.quads.iter().for_each(|q| grow(&mut acc, quad_bounds(q)));
    l.texts.iter().for_each(|t| grow(&mut acc, text_bounds(t)));
    l.images.iter().for_each(|i| grow(&mut acc, image_bounds(i)));
    acc
}

fn quad_bounds(q: &QuadItem) -> Rect {
    q.rect.expand(MARGIN).intersect(&q.clip)
}

/// Where a text run can draw: the renderer clips glyphs to this (see render.rs).
fn text_bounds(t: &TextItem) -> Rect {
    let area = if t.rect.w > 0.0 { t.rect } else { Rect { x: t.clip.x, w: t.clip.w, ..t.rect } };
    area.intersect(&t.clip).expand(MARGIN)
}

fn image_bounds(i: &ImageItem) -> Rect {
    i.rect.expand(MARGIN).intersect(&i.clip)
}

/// Clip to the screen, merge overlapping rectangles, and keep at most `MAX_RECTS`
/// (merging the pair that adds least). `None` when it covers most of the screen.
fn simplify(rects: Vec<Rect>, screen: (f32, f32)) -> Option<Vec<Rect>> {
    let whole = Rect { x: 0.0, y: 0.0, w: screen.0, h: screen.1 };
    let mut rects: Vec<Rect> = rects.iter().map(|r| r.intersect(&whole)).filter(|r| !r.is_empty()).collect();
    // Merge any two that overlap, until none do.
    'again: loop {
        for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                if !rects[i].intersect(&rects[j]).is_empty() {
                    let u = rects[i].union(&rects[j]);
                    rects.swap_remove(j);
                    rects[i] = u;
                    continue 'again;
                }
            }
        }
        break;
    }
    while rects.len() > MAX_RECTS {
        let mut best = (0, 1, f32::INFINITY);
        for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                let added = rects[i].union(&rects[j]).area() - rects[i].area() - rects[j].area();
                if added < best.2 {
                    best = (i, j, added);
                }
            }
        }
        let u = rects[best.0].union(&rects[best.1]);
        rects.swap_remove(best.1);
        rects[best.0] = u;
    }
    let area: f32 = rects.iter().map(Rect::area).sum();
    (area <= FULL_FRACTION * whole.area()).then_some(rects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Rgba;

    const SCREEN: (f32, f32) = (1000.0, 800.0);
    const GREY: Rgba = Rgba([200, 200, 200, 255]);

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    fn quad(r: Rect) -> QuadItem {
        QuadItem {
            rect: r,
            color: GREY,
            border_color: GREY,
            border_width: 0.0,
            radius: 6.0,
            clip: Rect::INFINITE,
        }
    }
    /// A window: a layer with a body and a title bar at (x, y).
    fn window(id: NodeId, x: f32, y: f32) -> Layer {
        Layer {
            key: (id, 0),
            quads: vec![quad(rect(x, y, 100.0, 80.0)), quad(rect(x, y, 100.0, 20.0))],
            ..Layer::default()
        }
    }
    fn desktop() -> Layer {
        Layer { key: (1, 0), quads: vec![quad(rect(0.0, 0.0, 1000.0, 800.0))], ..Layer::default() }
    }
    fn list(layers: Vec<Layer>) -> DrawList {
        DrawList { layers }
    }
    fn tracker_after(first: &DrawList) -> DamageTracker {
        let mut t = DamageTracker::new(3);
        assert_eq!(t.next_frame(first, SCREEN), None, "the first frame is drawn whole");
        t.drew(0);
        t
    }

    #[test]
    fn nothing_changed_means_nothing_to_draw() {
        let a = list(vec![desktop(), window(10, 50.0, 50.0)]);
        let mut t = tracker_after(&a);
        assert_eq!(t.next_frame(&a.clone(), SCREEN), Some(Vec::new()));
    }

    #[test]
    fn a_moved_window_damages_where_it_was_and_where_it_is() {
        let a = list(vec![desktop(), window(10, 50.0, 50.0)]);
        let b = list(vec![desktop(), window(10, 400.0, 300.0)]);
        let mut t = tracker_after(&a);
        let d = t.next_frame(&b, SCREEN).expect("partial");
        assert_eq!(d, vec![rect(48.0, 48.0, 104.0, 84.0), rect(398.0, 298.0, 104.0, 84.0)]);
    }

    #[test]
    fn a_small_move_merges_into_one_rectangle() {
        let a = list(vec![desktop(), window(10, 50.0, 50.0)]);
        let b = list(vec![desktop(), window(10, 60.0, 55.0)]);
        let mut t = tracker_after(&a);
        assert_eq!(t.next_frame(&b, SCREEN), Some(vec![rect(48.0, 48.0, 114.0, 89.0)]));
    }

    #[test]
    fn only_the_changed_item_is_damaged() {
        let a = list(vec![desktop(), window(10, 50.0, 50.0)]);
        let mut b = a.clone();
        b.layers[1].quads[1].color = Rgba([0, 0, 255, 255]); // the title bar turns blue
        let mut t = tracker_after(&a);
        assert_eq!(t.next_frame(&b, SCREEN), Some(vec![rect(48.0, 48.0, 104.0, 24.0)]));
    }

    #[test]
    fn changed_text_damages_its_box() {
        let text = |s: &str| TextItem {
            node: 7,
            slot: 0,
            text: s.into(),
            size: 13.0,
            color: GREY,
            bold: false,
            center: false,
            mono: false,
            rect: rect(300.0, 10.0, 200.0, 20.0),
            clip: Rect::INFINITE,
        };
        let mut a = list(vec![desktop()]);
        a.layers[0].texts.push(text("12:00"));
        let mut b = a.clone();
        b.layers[0].texts[0] = text("12:01");
        let mut t = tracker_after(&a);
        assert_eq!(t.next_frame(&b, SCREEN), Some(vec![rect(298.0, 8.0, 204.0, 24.0)]));
    }

    #[test]
    fn opening_closing_and_raising_windows() {
        let a = list(vec![desktop(), window(10, 50.0, 50.0), window(20, 600.0, 400.0)]);
        // A window opens on top.
        let opened =
            list(vec![desktop(), window(10, 50.0, 50.0), window(20, 600.0, 400.0), window(30, 300.0, 300.0)]);
        let mut t = tracker_after(&a);
        assert_eq!(t.next_frame(&opened, SCREEN), Some(vec![rect(298.0, 298.0, 104.0, 84.0)]));
        // It closes again.
        assert_eq!(t.next_frame(&a, SCREEN), Some(vec![rect(298.0, 298.0, 104.0, 84.0)]));
        // Raising window 10 above 20 changes both places in the stacking order.
        let raised = list(vec![desktop(), window(20, 600.0, 400.0), window(10, 50.0, 50.0)]);
        let mut d = t.next_frame(&raised, SCREEN).expect("partial");
        d.sort_by(|p, q| p.x.total_cmp(&q.x));
        assert_eq!(d, vec![rect(48.0, 48.0, 104.0, 84.0), rect(598.0, 398.0, 104.0, 84.0)]);
    }

    #[test]
    fn large_changes_and_new_screens_redraw_everything() {
        let a = list(vec![desktop(), window(10, 50.0, 50.0)]);
        let mut t = tracker_after(&a);
        // The desktop itself changing covers the whole screen.
        let mut b = a.clone();
        b.layers[0].quads[0].color = Rgba([0, 0, 0, 255]);
        assert_eq!(t.next_frame(&b, SCREEN), None);
        // A new screen size.
        assert_eq!(t.next_frame(&b, (1200.0, 800.0)), None);
    }

    #[test]
    fn layers_that_cannot_be_matched_redraw_everything() {
        let mut t = tracker_after(&list(vec![desktop(), window(10, 50.0, 50.0)]));
        let twins = list(vec![desktop(), window(10, 50.0, 50.0), window(10, 300.0, 50.0)]);
        assert_eq!(t.next_frame(&twins, SCREEN), None);
    }

    #[test]
    fn each_buffer_gets_everything_since_it_was_drawn() {
        let frames: Vec<DrawList> =
            (0..4).map(|i| list(vec![desktop(), window(10, 50.0 + 200.0 * i as f32, 50.0)])).collect();
        let mut t = DamageTracker::new(3);
        // Frame 1 is whole, into buffer 0.
        assert_eq!(t.next_frame(&frames[0], SCREEN), None);
        assert_eq!(t.region_for(0, SCREEN), None);
        t.drew(0);
        // Frame 2 into buffer 1, never drawn: whole.
        t.next_frame(&frames[1], SCREEN).expect("partial");
        assert_eq!(t.region_for(1, SCREEN), None);
        t.drew(1);
        // Frame 3 into buffer 2: whole too. Frame 4 back into buffer 0, which holds
        // frame 1: the window's places in frames 1 to 4.
        t.next_frame(&frames[2], SCREEN).expect("partial");
        t.drew(2);
        t.next_frame(&frames[3], SCREEN).expect("partial");
        let region = t.region_for(0, SCREEN).expect("partial");
        for x in [50.0, 250.0, 450.0, 650.0] {
            assert!(
                region.iter().any(|r| r.contains(x + 50.0, 90.0)),
                "window at {x} not redrawn: {region:?}"
            );
        }
        assert!(region.len() <= MAX_RECTS);
        // The newest buffer only needs frame 4's damage.
        let newest = t.region_for(2, SCREEN).expect("partial");
        assert!(
            newest.iter().any(|r| r.contains(700.0, 90.0)) && !newest.iter().any(|r| r.contains(100.0, 90.0))
        );
    }

    #[test]
    fn a_buffer_older_than_the_history_is_redrawn_whole() {
        let mut t = tracker_after(&list(vec![desktop(), window(10, 0.0, 50.0)]));
        for i in 1..=HISTORY + 1 {
            t.next_frame(&list(vec![desktop(), window(10, i as f32 * 10.0, 50.0)]), SCREEN);
            t.drew(1);
        }
        assert_eq!(t.region_for(0, SCREEN), None);
        assert!(t.region_for(1, SCREEN).is_some());
    }

    #[test]
    fn simplify_keeps_at_most_four_rectangles() {
        let many: Vec<Rect> = (0..10).map(|i| rect(i as f32 * 90.0, 0.0, 20.0, 20.0)).collect();
        let r = simplify(many, SCREEN).expect("small");
        assert!(r.len() <= MAX_RECTS, "{r:?}");
        for i in 0..10 {
            assert!(r.iter().any(|x| x.contains(i as f32 * 90.0 + 10.0, 10.0)));
        }
    }
}
