//! Long text, shaped and laid out a paragraph at a time.
//!
//! cosmic-text keeps each paragraph's shaping and layout cached in its `Buffer`
//! (a paragraph is a `\n`-separated line, which may wrap onto several rows). But
//! giving a buffer new text throws every cache away, and `shape_until_scroll`
//! lays out every paragraph, so a text area used to redo the whole document for
//! each keystroke and each caret move.
//!
//! [`Paragraphs`] keeps one buffer per text instead. New text replaces only the
//! paragraphs that differ, so typing reshapes one. Paragraphs are laid out only as
//! far down as something needs: the caret, a click, the bottom of what's shown. A
//! paragraph's position needs the heights of those above it, so everything above
//! the place asked about is laid out too, once; the rest waits until it's
//! scrolled to.
//!
//! The desktop (carets, selections, clicks) and the renderer (drawing) each keep
//! their own, with the same fonts and metrics, so they agree on where text is.

use glyphon::cosmic_text::{Align, AttrsList, BufferLine, LineEnding, Scroll};
use glyphon::{Buffer, Cursor, FontSystem, LayoutRun, Metrics, Shaping};

use crate::style::Font;

pub struct Paragraphs {
    buffer: Buffer,
    font: Font,
    width: Option<f32>,
    align: Option<Align>,
}

impl Paragraphs {
    /// `text` in `font`, wrapped at `width` if given (`None` is one long row per
    /// paragraph), each row aligned by `align`. Nothing is shaped yet.
    pub fn new(
        fonts: &mut FontSystem,
        text: &str,
        font: &Font,
        width: Option<f32>,
        align: Option<Align>,
    ) -> Self {
        let mut buffer = Buffer::new(fonts, Metrics::new(font.size, font.size * 1.3));
        buffer.set_size(width, None);
        let attrs = crate::fonts::attrs(fonts.db(), font);
        buffer.set_text(text, &attrs, Shaping::Advanced, align);
        Paragraphs { buffer, font: font.clone(), width, align }
    }

    /// Whether these were laid out for `font`, `width` and `align`. Cached rows
    /// don't know the width they were wrapped at, so anything else needs new ones.
    pub fn fits(&self, font: &Font, width: Option<f32>, align: Option<Align>) -> bool {
        self.font == *font && self.width == width && self.align == align
    }

    /// Take `text`, replacing only the paragraphs that differ: those between the
    /// longest run of equal paragraphs at the start and the one at the end.
    pub fn set_text(&mut self, fonts: &mut FontSystem, text: &str) {
        let new: Vec<&str> = text.split('\n').collect();
        let lines = &mut self.buffer.lines;
        let old_len = lines.len();
        let prefix = lines.iter().zip(&new).take_while(|(l, t)| l.text() == **t).count();
        if prefix == old_len && prefix == new.len() {
            return;
        }
        let room = old_len.min(new.len()) - prefix;
        let suffix =
            lines.iter().rev().zip(new.iter().rev()).take(room).take_while(|(l, t)| l.text() == **t).count();
        let attrs = AttrsList::new(&crate::fonts::attrs(fonts.db(), &self.font));
        let inserted = new.len() - suffix - prefix;
        lines.splice(
            prefix..old_len - suffix,
            new[prefix..prefix + inserted].iter().map(|t| {
                let mut line = BufferLine::new(*t, LineEnding::Lf, attrs.clone(), Shaping::Advanced);
                line.set_align(self.align);
                line
            }),
        );
        // Every paragraph but the last ends in a line break. Only those next to
        // the change can have become, or stopped being, the last.
        let last = lines.len() - 1;
        for (i, line) in
            lines.iter_mut().enumerate().take(prefix + inserted + 1).skip(prefix.saturating_sub(1))
        {
            line.set_ending(if i == last { LineEnding::None } else { LineEnding::Lf });
        }
    }

    /// The buffer, its rows pointed at what [`Paragraphs::show`] last showed.
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn len(&self) -> usize {
        self.buffer.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.lines.is_empty()
    }

    /// Paragraph `line`'s height, laying it out if it isn't yet.
    fn height_of(&mut self, fonts: &mut FontSystem, line: usize) -> f32 {
        let row = self.buffer.metrics().line_height;
        self.buffer
            .line_layout(fonts, line)
            .map_or(0.0, |rows| rows.iter().map(|r| r.line_height_opt.unwrap_or(row)).sum())
    }

    /// The top of paragraph `line`, from the text's top.
    pub fn top_of(&mut self, fonts: &mut FontSystem, line: usize) -> f32 {
        (0..line.min(self.len())).map(|i| self.height_of(fonts, i)).sum()
    }

    /// The paragraph at `y` from the text's top, and that paragraph's top; the
    /// last one if `y` is past the end.
    pub fn line_at(&mut self, fonts: &mut FontSystem, y: f32) -> (usize, f32) {
        let mut top = 0.0;
        for i in 0..self.len() {
            let h = self.height_of(fonts, i);
            if y < top + h || i + 1 == self.len() {
                return (i, top);
            }
            top += h;
        }
        (0, 0.0)
    }

    /// The whole text's height: exact as far down as `through` (laying out the
    /// paragraphs to there) and for paragraphs already laid out, estimated for the
    /// rest from their length and the average character width so far. Scrolling
    /// asks only as far as it shows, so the estimate settles as the text is
    /// scrolled through, without laying all of it out to scroll at all.
    pub fn height_through(&mut self, fonts: &mut FontSystem, through: f32) -> f32 {
        let row = self.buffer.metrics().line_height;
        let (mut h, mut chars, mut width) = (0.0, 0_usize, 0.0_f32);
        let mut i = 0;
        while i < self.len() && h < through {
            for r in self.buffer.line_layout(fonts, i).unwrap_or_default() {
                h += r.line_height_opt.unwrap_or(row);
                width += r.w;
            }
            chars += self.buffer.lines[i].text().chars().count();
            i += 1;
        }
        let per_char = if chars > 0 && width > 0.0 { width / chars as f32 } else { self.font.size * 0.5 };
        for line in &self.buffer.lines[i..] {
            h += match line.layout_opt() {
                Some(rows) => rows.iter().map(|r| r.line_height_opt.unwrap_or(row)).sum(),
                None => {
                    let w = line.text().chars().count() as f32 * per_char;
                    let rows = match self.width {
                        Some(wrap) if wrap > 0.0 => (w / wrap).ceil().max(1.0),
                        _ => 1.0,
                    };
                    rows * row
                }
            };
        }
        h
    }

    /// Lay out paragraphs `first..=last` and point the buffer's rows at them.
    /// Returns `first`'s top: the rows' `line_top`s are measured from it.
    fn reach(&mut self, fonts: &mut FontSystem, first: usize, last: usize) -> f32 {
        let top = self.top_of(fonts, first);
        for i in first..=last.min(self.len().saturating_sub(1)) {
            self.buffer.line_layout(fonts, i);
        }
        self.buffer.set_scroll(Scroll::new(first, 0.0, 0.0));
        self.buffer.set_size(self.width, None);
        top
    }

    /// The rows of paragraphs `first..=last`, with the top they're measured from.
    pub fn rows(
        &mut self,
        fonts: &mut FontSystem,
        first: usize,
        last: usize,
    ) -> (f32, impl Iterator<Item = LayoutRun<'_>>) {
        let top = self.reach(fonts, first, last);
        (top, self.buffer.layout_runs().take_while(move |r| r.line_i <= last))
    }

    /// The paragraph and byte under (`x`, `y`), from the text's top left; `None`
    /// past the end.
    pub fn hit(&mut self, fonts: &mut FontSystem, x: f32, y: f32) -> Option<Cursor> {
        let (line, _) = self.line_at(fonts, y);
        let top = self.reach(fonts, line, line);
        self.buffer.hit(x, y - top)
    }

    /// Lay out what shows from `from` to `from + height` (measured from the text's
    /// top) and point the buffer's rows at it, for drawing: its first row is
    /// drawn at `from`.
    pub fn show(&mut self, fonts: &mut FontSystem, from: f32, height: f32) -> &Buffer {
        let (first, top) = self.line_at(fonts, from);
        let (mut i, mut bottom) = (first, top);
        while i < self.len() && bottom < from + height {
            bottom += self.height_of(fonts, i);
            i += 1;
        }
        self.buffer.set_scroll(Scroll::new(first, from - top, 0.0));
        self.buffer.set_size(self.width, Some(height));
        &self.buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> Font {
        Font { size: 14.0, ..Font::default() }
    }

    fn texts(p: &Paragraphs) -> Vec<&str> {
        p.buffer.lines.iter().map(|l| l.text()).collect()
    }

    #[test]
    fn new_text_replaces_only_the_paragraphs_that_differ() {
        let mut fonts = crate::fonts::ui_only();
        let mut p = Paragraphs::new(&mut fonts, "one\ntwo\nthree", &font(), Some(200.0), None);
        p.height_through(&mut fonts, f32::INFINITY);
        assert!(p.buffer.lines.iter().all(|l| l.layout_opt().is_some()));
        // Typing in the middle paragraph: only it loses its layout.
        p.set_text(&mut fonts, "one\ntwos\nthree");
        assert_eq!(texts(&p), ["one", "twos", "three"]);
        let laid: Vec<bool> = p.buffer.lines.iter().map(|l| l.layout_opt().is_some()).collect();
        assert_eq!(laid, [true, false, true]);
        // Splitting and joining paragraphs, at the start, middle and end.
        for text in
            ["one\ntw\nos\nthree", "x\none\ntwos\nthree", "one\nthree", "one\ntwo\nthree\n", "", "\n\n"]
        {
            p.set_text(&mut fonts, text);
            assert_eq!(texts(&p), text.split('\n').collect::<Vec<_>>(), "{text:?}");
            let endings: Vec<LineEnding> = p.buffer.lines.iter().map(|l| l.ending()).collect();
            let mut want = vec![LineEnding::Lf; endings.len()];
            *want.last_mut().unwrap() = LineEnding::None;
            assert_eq!(endings, want, "only the last paragraph has no line break: {text:?}");
        }
    }

    #[test]
    fn only_what_is_asked_for_is_laid_out() {
        let mut fonts = crate::fonts::ui_only();
        let text = (0..1000).map(|i| format!("paragraph {i}")).collect::<Vec<_>>().join("\n");
        let mut p = Paragraphs::new(&mut fonts, &text, &font(), Some(400.0), None);
        let laid = |p: &Paragraphs| p.buffer.lines.iter().filter(|l| l.layout_opt().is_some()).count();
        let row = (14.0_f32 * 1.3).ceil();
        // Drawing the top 200 pixels lays out about that many rows' worth.
        let shown: Vec<usize> = p.show(&mut fonts, 0.0, 200.0).layout_runs().map(|r| r.line_i).collect();
        assert_eq!(shown.first(), Some(&0));
        assert!(laid(&p) < 20, "{}", laid(&p));
        // Rows further down are where the heights above put them.
        let top = p.top_of(&mut fonts, 500);
        assert!((top - 500.0 * 14.0 * 1.3).abs() < 1.0, "{top}");
        let (line, at) = p.line_at(&mut fonts, top + 1.0);
        assert_eq!((line, at), (500, top));
        let shown: Vec<(usize, f32)> =
            p.show(&mut fonts, top, 3.0 * row).layout_runs().map(|r| (r.line_i, r.line_top)).collect();
        assert_eq!(shown[0], (500, 0.0), "drawn from the top of what's shown");
        assert!(laid(&p) < 520, "{}", laid(&p));
        let hit = p.hit(&mut fonts, 0.0, top + 2.0).unwrap();
        assert_eq!((hit.line, hit.index), (500, 0));
        // The height is exact through what's asked about and estimated below it,
        // laying out nothing more; asked through the end, it's exact.
        let row = 14.0_f32 * 1.3;
        let guess = p.height_through(&mut fonts, top);
        assert!(laid(&p) < 520, "{}", laid(&p));
        assert!((guess - 1000.0 * row).abs() < 10.0 * row, "{guess}");
        let h = p.height_through(&mut fonts, f32::INFINITY);
        assert_eq!(laid(&p), 1000);
        assert!((h - 1000.0 * row).abs() < 2.0, "{h}");
    }
}
