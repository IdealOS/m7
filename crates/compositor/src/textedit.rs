//! Text editing: a string with a caret and an optional selection, as text fields
//! and text areas use it. Positions are character (Unicode scalar) indexes, as in
//! Loro's text, so an edit can go to the database unchanged.
//!
//! Every change is also returned as a [`Splice`] (at, delete, insert), which is
//! what a field bound to a document sends to `dbd`.

/// One change to the text: at character `at`, delete `delete` characters, then
/// insert `insert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Splice {
    pub at: usize,
    pub delete: usize,
    pub insert: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Edit {
    text: String,
    /// The caret, as a character index.
    caret: usize,
    /// The other end of the selection, if there is one.
    anchor: Option<usize>,
}

/// How a caret move goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    WordLeft,
    WordRight,
    /// The start and end of the whole text.
    Home,
    End,
    /// The start and end of the caret's line (lines end at `\n`).
    LineStart,
    LineEnd,
    /// To a character index (a click, or up and down in a text area).
    To(usize),
}

impl Edit {
    /// `text` with the caret at its end.
    pub fn new(text: &str) -> Edit {
        Edit { text: text.to_owned(), caret: text.chars().count(), anchor: None }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// The selection as (start, end), if it's not empty.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.caret).then(|| (a.min(self.caret), a.max(self.caret)))
    }

    pub fn selected_text(&self) -> String {
        self.selection().map(|(a, b)| self.slice(a, b)).unwrap_or_default()
    }

    fn slice(&self, a: usize, b: usize) -> String {
        self.text.chars().skip(a).take(b - a).collect()
    }

    fn byte(&self, char_index: usize) -> usize {
        self.text.char_indices().nth(char_index).map_or(self.text.len(), |(i, _)| i)
    }

    /// Replace the whole text (from the app or the database) keeping the caret in range.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_owned();
        self.caret = self.caret.min(self.len());
        self.anchor = None;
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.len();
    }

    /// Move the caret; with `extend`, grow or shrink the selection instead of dropping it.
    pub fn move_caret(&mut self, m: Move, extend: bool) {
        let len = self.len();
        // Without Shift, Left and Right collapse a selection to its edge.
        if !extend && let Some((a, b)) = self.selection() {
            match m {
                Move::Left => {
                    self.caret = a;
                    self.anchor = None;
                    return;
                }
                Move::Right => {
                    self.caret = b;
                    self.anchor = None;
                    return;
                }
                _ => {}
            }
        }
        let chars: Vec<char> = self.text.chars().collect();
        let to = match m {
            Move::Left => self.caret.saturating_sub(1),
            Move::Right => (self.caret + 1).min(len),
            Move::Home => 0,
            Move::End => len,
            Move::LineStart => {
                let mut i = self.caret;
                while i > 0 && chars[i - 1] != '\n' {
                    i -= 1;
                }
                i
            }
            Move::LineEnd => {
                let mut i = self.caret;
                while i < len && chars[i] != '\n' {
                    i += 1;
                }
                i
            }
            Move::To(i) => i.min(len),
            Move::WordLeft => {
                let mut i = self.caret;
                while i > 0 && chars[i - 1].is_whitespace() {
                    i -= 1;
                }
                while i > 0 && !chars[i - 1].is_whitespace() {
                    i -= 1;
                }
                i
            }
            Move::WordRight => {
                let mut i = self.caret;
                while i < len && chars[i].is_whitespace() {
                    i += 1;
                }
                while i < len && !chars[i].is_whitespace() {
                    i += 1;
                }
                i
            }
        };
        if extend {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = to;
    }

    /// Type `s` over the selection (or at the caret).
    pub fn insert(&mut self, s: &str) -> Splice {
        let (at, end) = self.selection().unwrap_or((self.caret, self.caret));
        let (b0, b1) = (self.byte(at), self.byte(end));
        self.text.replace_range(b0..b1, s);
        self.caret = at + s.chars().count();
        self.anchor = None;
        Splice { at, delete: end - at, insert: s.to_owned() }
    }

    /// Delete the selection, or the character before the caret.
    pub fn backspace(&mut self) -> Option<Splice> {
        match self.selection() {
            Some(_) => Some(self.insert("")),
            None if self.caret > 0 => {
                self.anchor = Some(self.caret - 1);
                Some(self.insert(""))
            }
            None => None,
        }
    }

    /// Delete the selection, or the character after the caret.
    pub fn delete(&mut self) -> Option<Splice> {
        match self.selection() {
            Some(_) => Some(self.insert("")),
            None if self.caret < self.len() => {
                self.anchor = Some(self.caret + 1);
                Some(self.insert(""))
            }
            None => None,
        }
    }

    /// Delete from the caret to where `m` would move it (or the selection, if any).
    pub fn delete_to(&mut self, m: Move) -> Option<Splice> {
        if self.selection().is_none() {
            let from = self.caret;
            self.move_caret(m, false);
            if self.caret == from {
                return None;
            }
            self.anchor = Some(from);
        }
        Some(self.insert(""))
    }

    /// Emacs's Ctrl-K: delete to the end of the line, or the line break if the
    /// caret is already there. Returns what went, to yank back.
    pub fn kill_line(&mut self) -> Option<(String, Splice)> {
        let from = self.caret;
        self.move_caret(Move::LineEnd, false);
        if self.caret == from {
            self.move_caret(Move::Right, false);
        }
        if self.caret == from {
            return None;
        }
        self.anchor = Some(from);
        let killed = self.selected_text();
        Some((killed, self.insert("")))
    }

    /// Emacs's Ctrl-T: swap the characters either side of the caret (the last two,
    /// at the end of the text), leaving the caret after them.
    pub fn transpose(&mut self) -> Option<Splice> {
        let len = self.len();
        if len < 2 || self.caret == 0 {
            return None;
        }
        let at = if self.caret >= len { len - 2 } else { self.caret - 1 };
        let chars: Vec<char> = self.text.chars().collect();
        let swapped: String = [chars[at + 1], chars[at]].iter().collect();
        self.caret = at;
        self.anchor = Some(at + 2);
        let s = self.insert(&swapped);
        Some(s)
    }

    /// Remove and return the selected text.
    pub fn cut(&mut self) -> Option<(String, Splice)> {
        let text = self.selected_text();
        if text.is_empty() {
            return None;
        }
        Some((text, self.insert("")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_moving_and_deleting() {
        let mut e = Edit::new("hello");
        assert_eq!(e.insert(" world"), Splice { at: 5, delete: 0, insert: " world".into() });
        e.move_caret(Move::Home, false);
        assert_eq!(e.insert(">"), Splice { at: 0, delete: 0, insert: ">".into() });
        assert_eq!(e.text(), ">hello world");
        assert_eq!(e.backspace(), Some(Splice { at: 0, delete: 1, insert: "".into() }));
        assert_eq!(e.backspace(), None, "nothing before the caret");
        assert_eq!(e.delete(), Some(Splice { at: 0, delete: 1, insert: "".into() }));
        assert_eq!(e.text(), "ello world");
        e.move_caret(Move::End, false);
        assert_eq!(e.delete(), None);
    }

    #[test]
    fn selections_replace_and_collapse() {
        let mut e = Edit::new("one two three");
        e.move_caret(Move::Home, false);
        e.move_caret(Move::WordRight, true);
        assert_eq!(e.selected_text(), "one");
        assert_eq!(e.insert("1"), Splice { at: 0, delete: 3, insert: "1".into() });
        assert_eq!(e.text(), "1 two three");
        e.move_caret(Move::End, false);
        e.move_caret(Move::WordLeft, true);
        assert_eq!(e.selected_text(), "three");
        e.move_caret(Move::Left, false);
        assert_eq!((e.caret(), e.selection()), (6, None), "Left collapses to the start");
        e.select_all();
        let (cut, splice) = e.cut().unwrap();
        assert_eq!((cut.as_str(), splice.delete, e.text()), ("1 two three", 11, ""));
        assert!(e.cut().is_none());
    }

    #[test]
    fn positions_are_characters_not_bytes() {
        let mut e = Edit::new("héllo 😀");
        assert_eq!(e.caret(), 7);
        assert_eq!(e.backspace(), Some(Splice { at: 6, delete: 1, insert: "".into() }));
        e.move_caret(Move::To(2), false);
        e.insert("ü");
        assert_eq!(e.text(), "héüllo ");
        e.move_caret(Move::To(99), false);
        assert_eq!(e.caret(), 7, "clamped to the end");
        e.set_text("ab");
        assert_eq!(e.caret(), 2);
    }

    #[test]
    fn line_moves_kills_deletes_and_transposes() {
        let mut e = Edit::new("one two\nthree four");
        e.move_caret(Move::To(12), false); // in "three"
        e.move_caret(Move::LineStart, false);
        assert_eq!(e.caret(), 8);
        e.move_caret(Move::LineEnd, false);
        assert_eq!(e.caret(), 18);
        e.move_caret(Move::To(4), false);
        e.move_caret(Move::LineEnd, true);
        assert_eq!(e.selected_text(), "two");
        // Kill to the line's end, then the line break itself.
        e.move_caret(Move::To(4), false);
        assert_eq!(e.kill_line().unwrap().0, "two");
        assert_eq!(e.kill_line().unwrap().0, "\n");
        assert_eq!(e.text(), "one three four");
        // Delete a word back, and to the line's start.
        e.move_caret(Move::End, false);
        e.delete_to(Move::WordLeft);
        assert_eq!(e.text(), "one three ");
        e.delete_to(Move::LineStart);
        assert_eq!(e.text(), "");
        assert!(e.delete_to(Move::LineStart).is_none());
        // Transpose: the two characters either side, or the last two at the end.
        let mut e = Edit::new("ab");
        e.transpose();
        assert_eq!((e.text(), e.caret()), ("ba", 2));
        e.move_caret(Move::To(1), false);
        e.transpose();
        assert_eq!((e.text(), e.caret()), ("ab", 2));
    }
}
