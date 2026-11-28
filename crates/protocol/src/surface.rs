//! Surfaces: pixels drawn by another process, shown in a window.
//!
//! Most of a window is drawn by the compositor from the app's scene graph. A
//! `surface` node is the exception: its pixels come from a service (`web`, whose
//! pages Servo draws, is the first), which the app names with the node's
//! `service` and `id` props. As with a terminal, the compositor attaches to the
//! service itself, acting for the app, so frames and input never pass through it.
//!
//! `attach` is a streamed request. The service answers with a [`SurfacePart::Buffers`]
//! part carrying shared-memory descriptors, one per buffer, and then a
//! [`SurfacePart::Frame`] part each time it has drawn into one. The compositor
//! copies the frame out and hands the buffer back with `release`; until then the
//! service draws into another. A size change (`resize`) brings new buffers.
//!
//! What's typed and pointed at while the node has focus or the pointer goes to the
//! service as `input`, in the surface's own pixels (its buffers' pixels), so the
//! service never needs to know where the window is. It is told how many of those
//! pixels make one of the screen's logical pixels (`scale`, 2 on a Retina
//! display), for content that sizes itself in such units, as a page's CSS does.
//!
//! The service checks that the session is the app's: an app may only show its own,
//! and only a system service may act for an app (`"as"`: the app's id, or
//! `conn:N` for a tool's connection, as an app run with `app-host --bundle` is).

use serde::{Deserialize, Serialize};

/// The largest surface, in pixels on a side.
pub const MAX_SIDE: u32 = 4096;
/// Most buffers one surface has at once.
pub const MAX_BUFFERS: u32 = 3;

/// The methods every surface service has, beside its own.
///
/// | method    | params             | result |
/// |-----------|--------------------|--------|
/// | `attach`  | [`AttachParams`]   | streamed [`SurfacePart`]s |
/// | `resize`  | [`SizeParams`]     | null |
/// | `release` | [`ReleaseParams`]  | null |
/// | `input`   | [`InputParams`]    | null |
pub mod method {
    pub const ATTACH: &str = "attach";
    pub const RESIZE: &str = "resize";
    pub const RELEASE: &str = "release";
    pub const INPUT: &str = "input";
}

/// Params of `attach`: the session, the size it's shown at (in pixels), and
/// how many of those pixels make a logical one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachParams {
    pub id: String,
    pub width: u32,
    pub height: u32,
    #[serde(default = "one")]
    pub scale: f32,
}

fn one() -> f32 {
    1.0
}

/// Params of `resize`.
pub type SizeParams = AttachParams;

/// Params of `release`: frame `seq`, in buffer `buffer`, has been copied out,
/// and the buffer may be drawn into. (The frame's number keeps a release that
/// crossed a resize from freeing a new buffer of the same number.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReleaseParams {
    pub id: String,
    pub buffer: u32,
    pub seq: u64,
}

/// A part of `attach`'s streamed answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "part", rename_all = "lowercase")]
pub enum SurfacePart {
    /// New buffers, replacing any before: `count` shared-memory descriptors come
    /// with this part, each `stride * height` bytes of 8-bit RGBA, rows top first.
    Buffers { width: u32, height: u32, stride: u32, count: u32 },
    /// Buffer `buffer` holds a new frame. `seq` counts frames.
    Frame { buffer: u32, seq: u64 },
}

/// Params of `input`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct InputParams {
    pub id: String,
    pub event: SurfaceInput,
}

/// Which mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Middle,
    Right,
}

/// Modifier keys held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Mods {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shift: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ctrl: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alt: bool,
    /// Command on a Mac keyboard, the Windows key on others.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub logo: bool,
}

/// Something the user did to a surface. Positions are in the surface's pixels,
/// from its top left.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SurfaceInput {
    /// The pointer moved to (x, y).
    Move { x: f32, y: f32 },
    /// A button went down or up at (x, y).
    Button { x: f32, y: f32, button: Button, pressed: bool },
    /// The wheel or trackpad scrolled by (dx, dy) pixels at (x, y); positive `dy`
    /// moves the view down the page, as the web's `deltaY` does.
    Wheel { x: f32, y: f32, dx: f32, dy: f32 },
    /// A key went down or up. `key` is named as the web's `KeyboardEvent.key`: the
    /// character it types (`"a"`, `"A"`, `" "`), or a name (`"Enter"`, `"ArrowLeft"`).
    Key { key: String, pressed: bool, mods: Mods },
    /// The surface gained or lost the keyboard.
    Focus { on: bool },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parts_and_input_read_as_documented() {
        let p: SurfacePart = serde_json::from_value(
            json!({"part": "buffers", "width": 4, "height": 2, "stride": 16, "count": 2}),
        )
        .unwrap();
        assert_eq!(p, SurfacePart::Buffers { width: 4, height: 2, stride: 16, count: 2 });
        let i: SurfaceInput = serde_json::from_value(
            json!({"kind": "key", "key": "Enter", "pressed": true, "mods": {"shift": true}}),
        )
        .unwrap();
        assert_eq!(
            i,
            SurfaceInput::Key {
                key: "Enter".into(),
                pressed: true,
                mods: Mods { shift: true, ..Mods::default() }
            }
        );
        assert_eq!(serde_json::to_value(Mods::default()).unwrap(), json!({}));
    }
}
