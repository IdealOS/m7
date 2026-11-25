//! Terminals: `ptyd` runs shells on pseudo-terminals and keeps their screens.
//!
//! ptyd claims `pty`. An app (the terminal) opens a session with `open`, a
//! streamed request that lasts as long as the shell: its first part says the
//! session's id, later parts say the shell set a title or exited, and cancelling
//! it hangs up the shell. The app then shows a `terminal` node with that id; the
//! compositor attaches to the session itself (`attach`, as the app), draws the
//! screen frames ptyd streams, and sends what's typed (`write`) and the size that
//! fits (`resize`). So a terminal's output never passes through the app.
//!
//! ptyd parses the shell's output (VT100 and the xterm extensions shells and
//! full-screen programs use) into a grid of cells, and streams that grid: the
//! whole screen first, then the rows that changed, a frame at most every 16 ms.
//!
//! A session belongs to whoever opened it; only they (or a system service acting
//! for them with `"as"`) may attach, write or resize. The shell runs outside any
//! app sandbox, as the user, which is the point of a terminal: apps reach `pty`
//! only with the `service:pty` grant.

use serde::{Deserialize, Serialize};

/// The name ptyd claims.
pub const SERVICE: &str = "pty";

/// Most sessions ptyd keeps at once.
pub const MAX_SESSIONS: usize = 32;
/// The largest screen, in cells on a side.
pub const MAX_CELLS: u16 = 1000;
/// Most bytes one `write` may carry.
pub const MAX_WRITE_BYTES: usize = 64 * 1024;

/// ptyd's methods.
///
/// | method   | params           | result |
/// |----------|------------------|--------|
/// | `open`   | [`OpenParams`]   | streamed [`SessionEvent`]s |
/// | `attach` | [`IdParams`]     | streamed [`ScreenFrame`]s |
/// | `write`  | [`WriteParams`]  | null |
/// | `resize` | [`ResizeParams`] | null |
pub mod method {
    pub const OPEN: &str = "open";
    pub const ATTACH: &str = "attach";
    pub const WRITE: &str = "write";
    pub const RESIZE: &str = "resize";
}

/// Params of `open`: the screen's starting size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpenParams {
    pub cols: u16,
    pub rows: u16,
}

/// A part of `open`'s streamed answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum SessionEvent {
    /// The first part: the session's id, for `attach` and the `terminal` node.
    Opened { id: String },
    /// The shell set the window title.
    Title { title: String },
    /// The shell exited, with this status (or the signal that ended it, negated).
    /// The stream ends after this.
    Exited { code: i32 },
}

/// Params of `attach`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IdParams {
    pub id: String,
}

/// Params of `write`: bytes for the shell, as text (keys are text, or escape
/// sequences, which are too).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WriteParams {
    pub id: String,
    pub data: String,
}

/// Params of `resize`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResizeParams {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
}

/// A run of cells on one row that look alike.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Run {
    /// One character per cell.
    pub text: String,
    /// `#rrggbb`; absent for the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub underline: bool,
}

/// One part of `attach`'s streamed answer: the screen, or what changed on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScreenFrame {
    pub cols: u16,
    pub rows: u16,
    /// Changed rows: each row's index and its runs, left to right. A row's runs
    /// cover it from the first column; the rest of the row is blank.
    pub lines: Vec<(u16, Vec<Run>)>,
    /// Where the cursor is (row, column), or absent while it's hidden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<(u16, u16)>,
    /// The arrow keys should send application-mode sequences (`ESC O A`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub app_cursor: bool,
    /// Pasted text should be wrapped in `ESC [200~` ... `ESC [201~`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bracketed_paste: bool,
}
