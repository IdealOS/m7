//! A terminal emulator: a shell's output in, a grid of cells out.
//!
//! The `vte` crate splits the byte stream into printable characters, control
//! characters and escape sequences; [`Terminal`] carries them out on a grid. It
//! covers what a shell and full-screen programs (BusyBox `vi`, `top`, `less`)
//! use: cursor movement and saving, erasing, inserting and deleting characters
//! and lines, a scrolling region, the alternate screen, SGR colors (the 16, the
//! 256 and 24-bit), autowrap, application cursor keys, bracketed paste, the
//! window title, and the cursor position and device attribute reports.
//!
//! Each character takes one cell; wide characters (CJK, most emoji) are drawn in
//! one too, overlapping the next.
//!
//! [`Terminal::frame`] reports the rows that changed since the last frame, as
//! runs of alike cells, so a screen that's mostly still costs little to stream.

use protocol::pty::{Run, ScreenFrame};
use vte::{Params, Parser, Perform};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: Color,
    bg: Color,
    bold: bool,
    underline: bool,
    inverse: bool,
}

const PLAIN: Style =
    Style { fg: Color::Default, bg: Color::Default, bold: false, underline: false, inverse: false };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    c: char,
    style: Style,
}

/// A blank cell with `style`'s background (erasing paints the current background).
fn blank(style: Style) -> Cell {
    Cell {
        c: ' ',
        style: Style { bold: false, underline: false, inverse: false, fg: Color::Default, ..style },
    }
}

/// The xterm palette for the first 16 colors.
const ANSI: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0xcd, 0x31, 0x31),
    (0x0d, 0xbc, 0x79),
    (0xe5, 0xe5, 0x10),
    (0x24, 0x72, 0xc8),
    (0xbc, 0x3f, 0xbc),
    (0x11, 0xa8, 0xcd),
    (0xe5, 0xe5, 0xe5),
    (0x66, 0x66, 0x66),
    (0xf1, 0x4c, 0x4c),
    (0x23, 0xd1, 0x8b),
    (0xf5, 0xf5, 0x43),
    (0x3b, 0x8e, 0xea),
    (0xd6, 0x70, 0xd6),
    (0x29, 0xb8, 0xdb),
    (0xff, 0xff, 0xff),
];

/// The screen's own colors: light text on dark.
pub const DEFAULT_FG: &str = "#e5e7eb";
pub const DEFAULT_BG: &str = "#111827";

fn rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => ANSI[i as usize],
        16..=231 => {
            let i = i - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (step(i / 36), step(i / 6 % 6), step(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

fn hex(c: Color, bold: bool) -> Option<String> {
    let (r, g, b) = match c {
        Color::Default => return None,
        // Bold brightens the first 8 colors, as xterm does.
        Color::Indexed(i) if bold && i < 8 => rgb(i + 8),
        Color::Indexed(i) => rgb(i),
        Color::Rgb(r, g, b) => (r, g, b),
    };
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}

/// The emulator: the parser and the screen it drives.
pub struct Emulator {
    parser: Parser,
    pub term: Terminal,
}

impl Emulator {
    pub fn new(cols: u16, rows: u16) -> Emulator {
        Emulator { parser: Parser::new(), term: Terminal::new(cols, rows) }
    }

    /// Carry out the shell's output `bytes`.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }
}

pub struct Terminal {
    cols: usize,
    rows: usize,
    grid: Vec<Vec<Cell>>,
    /// The main screen, put aside while the alternate one shows.
    primary: Option<Vec<Vec<Cell>>>,
    row: usize,
    col: usize,
    /// The last column was just written: the next character wraps first.
    wrap_next: bool,
    style: Style,
    saved: (usize, usize, Style),
    /// The scrolling region, inclusive.
    top: usize,
    bottom: usize,
    autowrap: bool,
    cursor_visible: bool,
    app_cursor: bool,
    bracketed_paste: bool,
    dirty: Vec<bool>,
    /// Something besides rows changed (the cursor, a mode).
    moved: bool,
    title: Option<String>,
    /// Replies to send back to the shell (cursor position, device attributes).
    replies: Vec<u8>,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Terminal {
        let (cols, rows) = (cols.max(1) as usize, rows.max(1) as usize);
        Terminal {
            cols,
            rows,
            grid: vec![vec![blank(PLAIN); cols]; rows],
            primary: None,
            row: 0,
            col: 0,
            wrap_next: false,
            style: PLAIN,
            saved: (0, 0, PLAIN),
            top: 0,
            bottom: rows - 1,
            autowrap: true,
            cursor_visible: true,
            app_cursor: false,
            bracketed_paste: false,
            dirty: vec![true; rows],
            moved: true,
            title: None,
            replies: Vec::new(),
        }
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols as u16, self.rows as u16)
    }

    /// The text of row `row`, without trailing blanks (for tests and logs).
    pub fn line(&self, row: usize) -> String {
        self.grid[row].iter().map(|c| c.c).collect::<String>().trim_end().to_owned()
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// A title the shell set since the last call.
    pub fn take_title(&mut self) -> Option<String> {
        self.title.take()
    }

    /// Bytes to write back to the shell.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// Change the screen's size, keeping what fits from the top left.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1) as usize, rows.max(1) as usize);
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        let fit = |grid: &mut Vec<Vec<Cell>>| {
            // Keep the bottom of a screen that's getting shorter (where the prompt is).
            let drop = grid.len().saturating_sub(rows);
            grid.drain(..drop);
            grid.resize(rows, vec![blank(PLAIN); cols]);
            for line in grid.iter_mut() {
                line.resize(cols, blank(PLAIN));
            }
        };
        let drop = self.rows.saturating_sub(rows);
        fit(&mut self.grid);
        if let Some(p) = &mut self.primary {
            fit(p);
        }
        self.cols = cols;
        self.rows = rows;
        self.row = self.row.saturating_sub(drop).min(rows - 1);
        self.col = self.col.min(cols - 1);
        self.top = 0;
        self.bottom = rows - 1;
        self.wrap_next = false;
        self.dirty = vec![true; rows];
        self.moved = true;
    }

    /// What changed since the last frame (everything, with `full`), or `None`
    /// if nothing did.
    pub fn frame(&mut self, full: bool) -> Option<ScreenFrame> {
        if !full && !self.moved && !self.dirty.iter().any(|d| *d) {
            return None;
        }
        let rows: Vec<usize> = (0..self.rows).filter(|&r| full || self.dirty[r]).collect();
        let frame = self.frame_of(&rows);
        self.dirty.iter_mut().for_each(|d| *d = false);
        self.moved = false;
        Some(frame)
    }

    /// The whole screen, without taking what changed (for a new viewer).
    pub fn snapshot(&self) -> ScreenFrame {
        self.frame_of(&(0..self.rows).collect::<Vec<_>>())
    }

    fn frame_of(&self, rows: &[usize]) -> ScreenFrame {
        ScreenFrame {
            cols: self.cols as u16,
            rows: self.rows as u16,
            lines: rows.iter().map(|&r| (r as u16, self.runs(r))).collect(),
            cursor: self.cursor_visible.then_some((self.row as u16, self.col as u16)),
            app_cursor: self.app_cursor,
            bracketed_paste: self.bracketed_paste,
        }
    }

    /// Row `row` as runs of alike cells, trailing default blanks left out.
    fn runs(&self, row: usize) -> Vec<Run> {
        let line = &self.grid[row];
        let end = line
            .iter()
            .rposition(|c| c.c != ' ' || c.style.bg != Color::Default || c.style.inverse)
            .map_or(0, |i| i + 1);
        let mut runs: Vec<Run> = Vec::new();
        let mut last: Option<Style> = None;
        for cell in &line[..end] {
            let s = cell.style;
            if last == Some(s) {
                runs.last_mut().expect("a run").text.push(cell.c);
                continue;
            }
            let (fg, bg) = if s.inverse {
                (
                    Some(hex(s.bg, false).unwrap_or_else(|| DEFAULT_BG.into())),
                    Some(hex(s.fg, s.bold).unwrap_or_else(|| DEFAULT_FG.into())),
                )
            } else {
                (hex(s.fg, s.bold), hex(s.bg, false))
            };
            runs.push(Run { text: cell.c.to_string(), fg, bg, bold: s.bold, underline: s.underline });
            last = Some(s);
        }
        runs
    }

    fn touch(&mut self, row: usize) {
        self.dirty[row] = true;
    }

    fn touch_all(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = true);
    }

    fn put(&mut self, c: char) {
        if self.wrap_next {
            self.wrap_next = false;
            if self.autowrap {
                self.col = 0;
                self.line_feed();
            }
        }
        let style = self.style;
        let (row, col) = (self.row, self.col);
        self.grid[row][col] = Cell { c, style };
        self.touch(row);
        if self.col + 1 < self.cols {
            self.col += 1;
        } else {
            self.wrap_next = true;
        }
        self.moved = true;
    }

    /// Down a line, scrolling the region at its bottom.
    fn line_feed(&mut self) {
        if self.row == self.bottom {
            self.scroll_up(1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
        self.moved = true;
    }

    /// Up a line, scrolling the region down at its top.
    fn reverse_index(&mut self) {
        if self.row == self.top {
            self.scroll_down(1);
        } else if self.row > 0 {
            self.row -= 1;
        }
        self.moved = true;
    }

    /// Scroll the region up `n` lines (new blank lines at its bottom).
    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.bottom - self.top + 1);
        let fill = blank(self.style);
        for _ in 0..n {
            self.grid.remove(self.top);
            self.grid.insert(self.bottom, vec![fill; self.cols]);
        }
        (self.top..=self.bottom).for_each(|r| self.touch(r));
    }

    /// Scroll the region down `n` lines (new blank lines at its top).
    fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.bottom - self.top + 1);
        let fill = blank(self.style);
        for _ in 0..n {
            self.grid.remove(self.bottom);
            self.grid.insert(self.top, vec![fill; self.cols]);
        }
        (self.top..=self.bottom).for_each(|r| self.touch(r));
    }

    fn goto(&mut self, row: usize, col: usize) {
        self.row = row.min(self.rows - 1);
        self.col = col.min(self.cols - 1);
        self.wrap_next = false;
        self.moved = true;
    }

    fn erase(&mut self, row: usize, from: usize, to: usize) {
        let fill = blank(self.style);
        for c in &mut self.grid[row][from.min(self.cols)..to.min(self.cols)] {
            *c = fill;
        }
        self.touch(row);
    }

    fn switch_screen(&mut self, alternate: bool) {
        match (alternate, self.primary.is_some()) {
            (true, false) => {
                let fresh = vec![vec![blank(PLAIN); self.cols]; self.rows];
                self.primary = Some(std::mem::replace(&mut self.grid, fresh));
            }
            (false, true) => {
                self.grid = self.primary.take().expect("checked");
            }
            _ => return,
        }
        self.touch_all();
    }

    fn sgr(&mut self, params: &Params) {
        let flat: Vec<u16> = params.iter().flat_map(|p| p.iter().copied()).collect();
        if flat.is_empty() {
            self.style = PLAIN;
            return;
        }
        let mut i = 0;
        while i < flat.len() {
            let p = flat[i];
            match p {
                0 => self.style = PLAIN,
                1 => self.style.bold = true,
                4 => self.style.underline = true,
                7 => self.style.inverse = true,
                22 => self.style.bold = false,
                24 => self.style.underline = false,
                27 => self.style.inverse = false,
                30..=37 => self.style.fg = Color::Indexed((p - 30) as u8),
                39 => self.style.fg = Color::Default,
                40..=47 => self.style.bg = Color::Indexed((p - 40) as u8),
                49 => self.style.bg = Color::Default,
                90..=97 => self.style.fg = Color::Indexed((p - 90 + 8) as u8),
                100..=107 => self.style.bg = Color::Indexed((p - 100 + 8) as u8),
                38 | 48 => {
                    let color = match flat.get(i + 1) {
                        Some(5) => {
                            i += 2;
                            flat.get(i).map(|&n| Color::Indexed(n.min(255) as u8))
                        }
                        Some(2) => {
                            let c = (flat.get(i + 2), flat.get(i + 3), flat.get(i + 4));
                            i += 4;
                            match c {
                                (Some(&r), Some(&g), Some(&b)) => Some(Color::Rgb(r as u8, g as u8, b as u8)),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some(c) = color {
                        if p == 38 {
                            self.style.fg = c;
                        } else {
                            self.style.bg = c;
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

impl Perform for Terminal {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.line_feed(),
            b'\r' => self.goto(self.row, 0),
            0x08 => self.goto(self.row, self.col.saturating_sub(1)),
            b'\t' => {
                let next = ((self.col / 8) + 1) * 8;
                self.goto(self.row, next.min(self.cols - 1));
            }
            _ => {} // BEL and the rest
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // 0 and 2 set the title.
        if let [kind, title, ..] = params
            && (*kind == b"0" || *kind == b"2")
        {
            self.title = Some(String::from_utf8_lossy(title).chars().take(200).collect());
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            ([], b'7') => self.saved = (self.row, self.col, self.style),
            ([], b'8') => {
                let (r, c, s) = self.saved;
                self.goto(r, c);
                self.style = s;
            }
            ([], b'D') => self.line_feed(),
            ([], b'E') => {
                self.goto(self.row, 0);
                self.line_feed();
            }
            ([], b'M') => self.reverse_index(),
            ([], b'c') => {
                let (cols, rows) = self.size();
                *self = Terminal::new(cols, rows);
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let args: Vec<u16> = params.iter().map(|p| p.first().copied().unwrap_or(0)).collect();
        // The nth argument, with 0 or absent meaning `default`.
        let arg = |n: usize, default: usize| {
            args.get(n).copied().filter(|&v| v != 0).map_or(default, |v| v as usize)
        };
        let private = intermediates.first() == Some(&b'?');
        match (action, private) {
            ('A', false) => self.goto(self.row.saturating_sub(arg(0, 1)), self.col),
            ('B', false) | ('e', false) => self.goto(self.row + arg(0, 1), self.col),
            ('C', false) | ('a', false) => self.goto(self.row, self.col + arg(0, 1)),
            ('D', false) => self.goto(self.row, self.col.saturating_sub(arg(0, 1))),
            ('E', false) => self.goto(self.row + arg(0, 1), 0),
            ('F', false) => self.goto(self.row.saturating_sub(arg(0, 1)), 0),
            ('G', false) | ('`', false) => self.goto(self.row, arg(0, 1) - 1),
            ('d', false) => self.goto(arg(0, 1) - 1, self.col),
            ('H', false) | ('f', false) => self.goto(arg(0, 1) - 1, arg(1, 1) - 1),
            ('J', false) => {
                let (row, col) = (self.row, self.col);
                match args.first().copied().unwrap_or(0) {
                    0 => {
                        self.erase(row, col, self.cols);
                        (row + 1..self.rows).for_each(|r| self.erase(r, 0, self.cols));
                    }
                    1 => {
                        (0..row).for_each(|r| self.erase(r, 0, self.cols));
                        self.erase(row, 0, col + 1);
                    }
                    _ => (0..self.rows).for_each(|r| self.erase(r, 0, self.cols)),
                }
            }
            ('K', false) => {
                let (row, col) = (self.row, self.col);
                match args.first().copied().unwrap_or(0) {
                    0 => self.erase(row, col, self.cols),
                    1 => self.erase(row, 0, col + 1),
                    _ => self.erase(row, 0, self.cols),
                }
            }
            ('X', false) => {
                let (row, col) = (self.row, self.col);
                self.erase(row, col, col + arg(0, 1));
            }
            ('@', false) => {
                let (row, col, n, fill) = (self.row, self.col, arg(0, 1), blank(self.style));
                let line = &mut self.grid[row];
                for _ in 0..n.min(self.cols - col) {
                    line.insert(col, fill);
                    line.pop();
                }
                self.touch(row);
            }
            ('P', false) => {
                let (row, col, n, fill) = (self.row, self.col, arg(0, 1), blank(self.style));
                let line = &mut self.grid[row];
                for _ in 0..n.min(self.cols - col) {
                    line.remove(col);
                    line.push(fill);
                }
                self.touch(row);
            }
            ('L', false) | ('M', false) if (self.top..=self.bottom).contains(&self.row) => {
                // Insert or delete lines: scroll the part of the region below the cursor.
                let top = self.top;
                self.top = self.row;
                if action == 'L' {
                    self.scroll_down(arg(0, 1));
                } else {
                    self.scroll_up(arg(0, 1));
                }
                self.top = top;
                self.goto(self.row, 0);
            }
            ('S', false) => self.scroll_up(arg(0, 1)),
            ('T', false) => self.scroll_down(arg(0, 1)),
            ('r', false) => {
                let top = arg(0, 1) - 1;
                let bottom = arg(1, self.rows).min(self.rows) - 1;
                if top < bottom {
                    self.top = top;
                    self.bottom = bottom;
                    self.goto(0, 0);
                }
            }
            ('m', false) => self.sgr(params),
            ('s', false) => self.saved = (self.row, self.col, self.style),
            ('u', false) => {
                let (r, c, s) = self.saved;
                self.goto(r, c);
                self.style = s;
            }
            ('n', false) if args.first() == Some(&6) => {
                self.replies.extend(format!("\x1b[{};{}R", self.row + 1, self.col + 1).into_bytes());
            }
            ('n', false) if args.first() == Some(&5) => self.replies.extend(b"\x1b[0n"),
            ('c', false) if intermediates.is_empty() => self.replies.extend(b"\x1b[?1;2c"),
            ('h' | 'l', true) => {
                let on = action == 'h';
                for &mode in &args {
                    match mode {
                        1 => self.app_cursor = on,
                        7 => self.autowrap = on,
                        25 => self.cursor_visible = on,
                        47 | 1047 => self.switch_screen(on),
                        1049 => {
                            if on {
                                self.saved = (self.row, self.col, self.style);
                                self.switch_screen(true);
                                (0..self.rows).for_each(|r| self.erase(r, 0, self.cols));
                            } else {
                                self.switch_screen(false);
                                let (r, c, s) = self.saved;
                                self.goto(r, c);
                                self.style = s;
                            }
                        }
                        2004 => self.bracketed_paste = on,
                        _ => {}
                    }
                }
                self.moved = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cols: u16, rows: u16, input: &str) -> Emulator {
        let mut e = Emulator::new(cols, rows);
        e.feed(input.as_bytes());
        e
    }

    fn lines(e: &Emulator) -> Vec<String> {
        (0..e.term.rows).map(|r| e.term.line(r)).collect()
    }

    #[test]
    fn text_wraps_and_scrolls() {
        let e = run(5, 3, "helloworld\r\nab\r\ncd");
        assert_eq!(lines(&e), ["world", "ab", "cd"], "hello scrolled off");
        let e = run(5, 2, "abcde");
        assert_eq!(e.term.cursor(), (0, 4), "at the last column, the wrap waits");
        let e = run(5, 2, "abcdef");
        assert_eq!(lines(&e), ["abcde", "f"]);
        let e = run(5, 2, "\x1b[?7labcdefg");
        assert_eq!(lines(&e), ["abcdg", ""], "without autowrap the last cell is overwritten");
    }

    #[test]
    fn control_characters_move_the_cursor() {
        let e = run(20, 2, "ab\tc\x08\x08X\rZ");
        assert_eq!(lines(&e), ["Zb     Xc", ""], "a tab stop at 8; backspaces don't erase");
        let e = run(10, 3, "a\nb");
        assert_eq!(lines(&e), ["a", " b", ""], "LF moves down, not back");
    }

    #[test]
    fn cursor_movement_and_erasing() {
        let e = run(10, 3, "0123456789\x1b[2;3Hxy\x1b[1;5H\x1b[K");
        assert_eq!(lines(&e), ["0123", "  xy", ""]);
        let e = run(10, 3, "aaaaaaaaaa\r\nbbbbbbbbbb\r\ncccccccccc\x1b[2;5H\x1b[1J");
        assert_eq!(lines(&e), ["", "     bbbbb", "cccccccccc"]);
        let e = run(10, 2, "abcdef\x1b[1;3H\x1b[2P");
        assert_eq!(lines(&e), ["abef", ""], "delete characters");
        let e = run(10, 2, "abcdef\x1b[1;3H\x1b[2@");
        assert_eq!(lines(&e), ["ab  cdef", ""], "insert blanks");
        let e = run(10, 2, "abc\x1b[5D\x1b[10C!");
        assert_eq!(lines(&e), ["abc      !", ""], "moves stop at the edges");
        let e = run(10, 3, "x\x1b[2J");
        assert_eq!(lines(&e), ["", "", ""]);
    }

    #[test]
    fn the_scrolling_region_and_line_insertion() {
        let mut e = run(4, 4, "1\r\n2\r\n3\r\n4\x1b[2;3r\x1b[3;1H\n");
        assert_eq!(lines(&e), ["1", "3", "", "4"], "only rows 2-3 scroll");
        e.feed(b"\x1b[r\x1b[2;1H\x1b[L");
        assert_eq!(lines(&e), ["1", "", "3", ""], "insert a line at row 2");
        e.feed(b"\x1b[1;1H\x1b[2M");
        assert_eq!(lines(&e), ["3", "", "", ""], "delete two lines");
        let e = run(3, 3, "a\r\nb\x1b[1;1H\x1bM");
        assert_eq!(lines(&e), ["", "a", "b"], "reverse index at the top scrolls down");
    }

    #[test]
    fn the_alternate_screen_keeps_the_main_one() {
        let mut e = run(10, 2, "$ vi\x1b[?1049h");
        assert_eq!(lines(&e), ["", ""]);
        e.feed(b"editing");
        e.feed(b"\x1b[?1049l");
        assert_eq!(lines(&e), ["$ vi", ""]);
        assert_eq!(e.term.cursor(), (0, 4), "the cursor comes back too");
    }

    #[test]
    fn colors_become_runs() {
        let mut e = run(20, 1, "a\x1b[31mred\x1b[1;38;5;21mB\x1b[0;7mI\x1b[27;48;2;1;2;3mx\x1b[m  ");
        let f = e.term.frame(true).unwrap();
        let runs = &f.lines[0].1;
        let texts: Vec<&str> = runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["a", "red", "B", "I", "x"], "trailing default blanks are left out");
        assert_eq!(runs[0].fg, None);
        assert_eq!(runs[1].fg.as_deref(), Some("#cd3131"));
        assert_eq!((runs[2].fg.as_deref(), runs[2].bold), (Some("#0000ff"), true));
        assert_eq!(
            (runs[3].fg.as_deref(), runs[3].bg.as_deref()),
            (Some(DEFAULT_BG), Some(DEFAULT_FG)),
            "inverse"
        );
        assert_eq!(runs[4].bg.as_deref(), Some("#010203"));
        let e2 = run(5, 1, "\x1b[1;31mx");
        let mut t = e2.term;
        assert_eq!(t.frame(true).unwrap().lines[0].1[0].fg.as_deref(), Some("#f14c4c"), "bold brightens");
    }

    #[test]
    fn frames_carry_only_what_changed() {
        let mut e = run(10, 3, "one\r\ntwo");
        let first = e.term.frame(false).unwrap();
        assert_eq!(first.lines.len(), 3, "everything starts dirty");
        assert_eq!(first.cursor, Some((1, 3)));
        assert!(e.term.frame(false).is_none(), "then nothing");
        e.feed(b"!");
        let f = e.term.frame(false).unwrap();
        assert_eq!(f.lines.iter().map(|l| l.0).collect::<Vec<_>>(), [1]);
        e.feed(b"\x1b[?25l\x1b[?1h\x1b[?2004h");
        let f = e.term.frame(false).unwrap();
        assert!(f.lines.is_empty() && f.cursor.is_none() && f.app_cursor && f.bracketed_paste);
    }

    #[test]
    fn titles_and_reports() {
        let mut e = run(10, 3, "\x1b]0;my shell\x07ab\x1b[6n\x1b[c");
        assert_eq!(e.term.take_title().as_deref(), Some("my shell"));
        assert_eq!(e.term.take_title(), None);
        assert_eq!(e.term.take_replies(), b"\x1b[1;3R\x1b[?1;2c");
    }

    #[test]
    fn resizing_keeps_the_bottom_and_the_cursor_inside() {
        let mut e = run(10, 4, "1\r\n2\r\n3\r\n4");
        e.term.resize(4, 2);
        assert_eq!(lines(&e), ["3", "4"]);
        assert_eq!(e.term.cursor(), (1, 1));
        e.term.resize(6, 3);
        assert_eq!(lines(&e), ["3", "4", ""]);
        e.feed(b"\x1b[3;6Hx");
        assert_eq!(e.term.line(2), "     x");
        let e = run(0, 0, "x");
        assert_eq!(e.term.size(), (1, 1), "never empty");
    }

    #[test]
    fn junk_never_panics() {
        let mut e = Emulator::new(7, 3);
        let mut seed: u64 = 1;
        for _ in 0..20_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let b = (seed >> 33) as u8;
            // Lean toward escape sequences.
            let b = if b.is_multiple_of(7) {
                0x1b
            } else if b.is_multiple_of(11) {
                b'['
            } else {
                b
            };
            e.feed(&[b]);
            if seed.is_multiple_of(997) {
                e.term.resize((seed % 9) as u16, (seed % 5) as u16);
            }
        }
        let _ = e.term.frame(true);
    }
}
