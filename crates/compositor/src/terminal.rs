//! The `terminal` node: a grid of cells fed by `ptyd`, not by the app.
//!
//! An app creates a `terminal` node naming a pty session (`pty`) it opened. The
//! compositor attaches to that session itself, as the app, and keeps the screen
//! here: each frame ptyd streams replaces the rows that changed. The rows go into
//! the node's `_lines` prop for drawing, with `_cursor` and `_cell` (a cell's
//! width and height, from the monospace font). Typing into a focused terminal
//! goes to ptyd as bytes ([`key_bytes`]), and a terminal whose node changes size
//! tells ptyd how many cells now fit ([`fit`]).

use protocol::pty::{Run, ScreenFrame};
use serde_json::{Value, json};

use crate::input::{Key, Mods};

/// Space between a terminal's edge and its cells.
pub const PAD: f32 = 6.0;
/// The default type size of a terminal's cells.
pub const SIZE: f32 = 14.0;
/// The screen's colors (ptyd sends `None` for these).
pub const FG: &str = "#e5e7eb";
pub const BG: &str = "#111827";
pub const CURSOR: &str = "#60a5fa";

/// One terminal node's screen and session.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub session: String,
    /// The app it's shown for, which the compositor acts as.
    pub app: Option<String>,
    pub rows: Vec<Vec<Run>>,
    pub cursor: Option<(u16, u16)>,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    /// The size last asked of ptyd.
    pub asked: Option<(u16, u16)>,
}

impl View {
    pub fn new(session: String, app: Option<String>) -> View {
        View {
            session,
            app,
            rows: Vec::new(),
            cursor: None,
            app_cursor: false,
            bracketed_paste: false,
            asked: None,
        }
    }

    /// Take frame `f`'s rows, cursor and modes.
    pub fn apply(&mut self, f: ScreenFrame) {
        self.rows.resize(f.rows as usize, Vec::new());
        for (row, runs) in f.lines {
            if let Some(r) = self.rows.get_mut(row as usize) {
                *r = runs;
            }
        }
        self.cursor = f.cursor;
        self.app_cursor = f.app_cursor;
        self.bracketed_paste = f.bracketed_paste;
    }

    /// The props that show this screen.
    pub fn props(&self) -> Value {
        json!({"_lines": self.rows, "_cursor": self.cursor})
    }

    /// Params for a request about this session, made as its app.
    pub fn params(&self, mut extra: Value) -> Value {
        extra["id"] = json!(self.session);
        if let Some(app) = &self.app {
            extra["as"] = json!(app);
        }
        extra
    }

    /// Text pasted into the terminal, as the shell should get it.
    pub fn paste(&self, text: &str) -> String {
        // A bracketed paste can't contain the end marker, or a pasted command
        // could break out of it.
        let text = text.replace("\x1b[201~", "");
        if self.bracketed_paste { format!("\x1b[200~{text}\x1b[201~") } else { text.replace('\n', "\r") }
    }
}

/// How many cells (columns, rows) fit in a `w` by `h` terminal with `cell`-sized cells.
pub fn fit(w: f32, h: f32, cell: (f32, f32)) -> (u16, u16) {
    let cols = ((w - 2.0 * PAD) / cell.0.max(1.0)).floor().clamp(1.0, 1000.0) as u16;
    let rows = ((h - 2.0 * PAD) / cell.1.max(1.0)).floor().clamp(1.0, 1000.0) as u16;
    (cols, rows)
}

/// What a key sends to the shell, as xterm does, or `None` for keys it ignores.
/// `app_cursor` is the shell's application cursor-key mode.
pub fn key_bytes(key: Key, mods: Mods, app_cursor: bool) -> Option<String> {
    let csi = |c: char| if app_cursor { format!("\x1bO{c}") } else { format!("\x1b[{c}") };
    let alt = |s: String| if mods.alt { format!("\x1b{s}") } else { s };
    Some(match key {
        Key::Enter => alt("\r".into()),
        Key::Tab if mods.shift => "\x1b[Z".into(),
        Key::Tab => "\t".into(),
        Key::Escape => "\x1b".into(),
        Key::Backspace => alt("\x7f".into()),
        Key::Delete => "\x1b[3~".into(),
        Key::Up => csi('A'),
        Key::Down => csi('B'),
        Key::Right => csi('C'),
        Key::Left => csi('D'),
        Key::Home => csi('H'),
        Key::End => csi('F'),
        Key::PageUp => "\x1b[5~".into(),
        Key::PageDown => "\x1b[6~".into(),
        Key::Char(c) if mods.ctrl => {
            // Ctrl with a letter or one of @[\]^_ is a control character.
            let upper = c.to_ascii_uppercase();
            match upper {
                '@'..='_' => alt(((upper as u8 - b'@') as char).to_string()),
                ' ' => alt("\0".into()),
                '?' => alt("\x7f".into()),
                _ => return None,
            }
        }
        Key::Char(c) => alt(c.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool, alt: bool, shift: bool) -> Mods {
        Mods { ctrl, alt, shift, logo: false }
    }

    #[test]
    fn keys_become_what_xterm_sends() {
        let none = Mods::default();
        assert_eq!(key_bytes(Key::Char('a'), none, false).as_deref(), Some("a"));
        assert_eq!(key_bytes(Key::Char('c'), mods(true, false, false), false).as_deref(), Some("\x03"));
        assert_eq!(key_bytes(Key::Char('D'), mods(true, false, true), false).as_deref(), Some("\x04"));
        assert_eq!(key_bytes(Key::Char('['), mods(true, false, false), false).as_deref(), Some("\x1b"));
        assert_eq!(key_bytes(Key::Char('b'), mods(false, true, false), false).as_deref(), Some("\x1bb"));
        assert_eq!(key_bytes(Key::Char('1'), mods(true, false, false), false), None);
        assert_eq!(key_bytes(Key::Enter, none, false).as_deref(), Some("\r"));
        assert_eq!(key_bytes(Key::Backspace, none, false).as_deref(), Some("\x7f"));
        assert_eq!(key_bytes(Key::Up, none, false).as_deref(), Some("\x1b[A"));
        assert_eq!(key_bytes(Key::Up, none, true).as_deref(), Some("\x1bOA"), "application cursor keys");
        assert_eq!(key_bytes(Key::Tab, mods(false, false, true), false).as_deref(), Some("\x1b[Z"));
        assert_eq!(key_bytes(Key::Delete, none, false).as_deref(), Some("\x1b[3~"));
    }

    #[test]
    fn frames_update_rows_and_cells_fit() {
        let mut v = View::new("1".into(), Some("org.x.term".into()));
        let run = |t: &str| Run { text: t.into(), fg: None, bg: None, bold: false, underline: false };
        v.apply(ScreenFrame {
            cols: 10,
            rows: 3,
            lines: vec![(0, vec![run("a")]), (2, vec![run("c")])],
            cursor: Some((2, 1)),
            app_cursor: false,
            bracketed_paste: true,
        });
        v.apply(ScreenFrame {
            cols: 10,
            rows: 3,
            lines: vec![(2, vec![run("cc")])],
            cursor: None,
            app_cursor: true,
            bracketed_paste: true,
        });
        assert_eq!(
            v.rows.iter().map(|r| r.iter().map(|x| x.text.as_str()).collect::<String>()).collect::<Vec<_>>(),
            ["a", "", "cc"]
        );
        assert_eq!((v.cursor, v.app_cursor), (None, true));
        assert_eq!(v.params(json!({"data": "x"})), json!({"data": "x", "id": "1", "as": "org.x.term"}));
        assert_eq!(v.paste("ls\n\x1b[201~rm"), "\x1b[200~ls\nrm\x1b[201~", "the end marker can't be pasted");
        assert_eq!(fit(100.0, 50.0, (8.0, 17.0)), (11, 2));
        assert_eq!(fit(0.0, 0.0, (8.0, 17.0)), (1, 1));
    }
}
