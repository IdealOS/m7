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
    Home,
    End,
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
}
