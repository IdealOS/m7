//! Playing music: `audiod` plays `Track` documents and owns the queue.
//!
//! audiod claims `audio`, and apps reach it with the `service:audio` grant. It
//! reads tracks as the app that asked (db's `as`), so an app can only queue the
//! tracks it may read; system services and tools act with audiod's own rights.
//! Playback lives in the service, not in an app, so music keeps playing when the
//! app that started it closes or stalls; an app, the top bar and `osctl` are all
//! remote controls, and follow what's playing with `watch`.
//!
//! The queue is in play order. Turning shuffle on shuffles what comes after the
//! current track, and turning it off puts the original order back. When a track
//! plays to its end, audiod adds one to its `playCount` and sets `lastPlayed`.
//!
//! Two `Setting` documents, which Settings writes, steer it, and it follows
//! them as they change: [`OUTPUT_KEY`] picks the sound output (one of those
//! `outputs` lists, by id; missing or `null` for the system's default), and
//! [`LEVELING_KEY`] how loud tracks play ([`Leveling`]).

use serde::{Deserialize, Serialize};

/// The name audiod claims.
pub const SERVICE: &str = "audio";

/// Most tracks the queue holds.
pub const MAX_QUEUE: usize = 10_000;

/// The `Setting` whose value is the id of the sound output to play to (as
/// `outputs` gives it), or `null` for the system's default.
pub const OUTPUT_KEY: &str = "audio.output";
/// The `Setting` whose value is a [`Leveling`]: `"off"`, `"track"` or `"album"`.
pub const LEVELING_KEY: &str = "audio.leveling";

/// audiod's methods.
///
/// | method     | params            | result |
/// |------------|-------------------|--------|
/// | `status`   | none              | [`AudioStatus`] |
/// | `watch`    | none              | streamed [`AudioStatus`]es: now, then each change |
/// | `play`     | [`PlayParams`]    | [`AudioStatus`]: the queue replaced, playing |
/// | `enqueue`  | [`EnqueueParams`] | [`AudioStatus`] |
/// | `pause`    | none              | [`AudioStatus`] |
/// | `resume`   | none              | [`AudioStatus`] |
/// | `toggle`   | none              | [`AudioStatus`]: paused if playing, else playing |
/// | `stop`     | none              | [`AudioStatus`]: stopped, the queue kept |
/// | `next`     | none              | [`AudioStatus`] |
/// | `previous` | none              | [`AudioStatus`]: back to the start of the track, or to the one before if near its start |
/// | `jump`     | [`IndexParams`]   | [`AudioStatus`]: playing that queue entry |
/// | `remove`   | [`IndexParams`]   | [`AudioStatus`] |
/// | `clear`    | none              | [`AudioStatus`]: stopped, the queue empty |
/// | `seek`     | [`SeekParams`]    | [`AudioStatus`] |
/// | `volume`   | [`VolumeParams`]  | [`AudioStatus`] |
/// | `shuffle`  | [`ShuffleParams`] | [`AudioStatus`] |
/// | `repeat`   | [`RepeatParams`]  | [`AudioStatus`] |
/// | `outputs`  | none              | [`OutputsResult`]: the sound outputs there are now |
pub mod method {
    pub const STATUS: &str = "status";
    pub const WATCH: &str = "watch";
    pub const PLAY: &str = "play";
    pub const ENQUEUE: &str = "enqueue";
    pub const PAUSE: &str = "pause";
    pub const RESUME: &str = "resume";
    pub const TOGGLE: &str = "toggle";
    pub const STOP: &str = "stop";
    pub const NEXT: &str = "next";
    pub const PREVIOUS: &str = "previous";
    pub const JUMP: &str = "jump";
    pub const REMOVE: &str = "remove";
    pub const CLEAR: &str = "clear";
    pub const SEEK: &str = "seek";
    pub const VOLUME: &str = "volume";
    pub const SHUFFLE: &str = "shuffle";
    pub const REPEAT: &str = "repeat";
    pub const OUTPUTS: &str = "outputs";
}

/// Error codes audiod sends, besides the broker's.
pub mod code {
    /// A track id that isn't a `Track` the caller may read, or a queue index past
    /// the end.
    pub const NOT_FOUND: &str = "not_found";
    /// Nothing is queued.
    pub const EMPTY: &str = "empty";
    /// More than [`super::MAX_QUEUE`] tracks.
    pub const TOO_MANY: &str = "too_many";
}

/// Whether music is playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum PlayState {
    Playing,
    Paused,
    /// Nothing playing: never started, stopped, or the queue ran out.
    Stopped,
}

/// What happens at the end of the queue, or of a track.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    /// Stop after the last track.
    #[default]
    Off,
    /// Start the queue again after the last track.
    All,
    /// Play the current track again and again.
    One,
}

/// How audiod evens out loudness between tracks, from their ReplayGain tags. A
/// track without them plays as it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Leveling {
    /// Every track as it was mastered.
    Off,
    /// Each track at the same loudness.
    #[default]
    Track,
    /// Each album at the same loudness, its tracks as loud as each other as the
    /// album has them (a track without an album gain is levelled on its own).
    Album,
}

/// A sound output audiod can play to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OutputDevice {
    /// What the `audio.output` setting holds to choose it.
    pub id: String,
    /// What to show: "Headphones", "HDMI 1", a USB device's name.
    pub name: String,
}

/// Result of `outputs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OutputsResult {
    pub outputs: Vec<OutputDevice>,
    /// The id of the one playing now, if it's one of them (not the system's
    /// default, or nowhere).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

/// One entry of the queue: a track and what to show for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct QueueItem {
    /// The `Track` document's id.
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Seconds, from its tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Its cover's blob hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork: Option<String>,
}

/// Result of most methods, and each part of `watch`.
///
/// Where the current track is: `position` seconds at `at` (milliseconds since the
/// Unix epoch). While playing, add the time since `at`; watchers aren't sent a
/// part for every second that passes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioStatus {
    pub state: PlayState,
    /// In play order.
    pub queue: Vec<QueueItem>,
    /// The index in `queue` of the track playing, paused, or to play next when
    /// stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<u32>,
    pub position: f64,
    pub at: i64,
    /// The current track's length in seconds, from its audio once it's playing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// 0 (silent) to 1.
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// The sound output in use, or none when there's nowhere to play.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The last thing that went wrong, such as a track that couldn't be played
    /// (and was skipped). Cleared by the next `play`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Params of `play`: replace the queue with `tracks` and play from `start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlayParams {
    /// `Track` document ids.
    pub tracks: Vec<String>,
    /// Which of them to start with (default 0, the first).
    #[serde(default)]
    pub start: u32,
}

/// Params of `enqueue`: add `tracks` at the end of the queue, or right after the
/// current track with `next`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EnqueueParams {
    pub tracks: Vec<String>,
    #[serde(default)]
    pub next: bool,
}

/// Params of `jump` and `remove`: an index into the queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IndexParams {
    pub index: u32,
}

/// Params of `seek`: seconds from the start of the current track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SeekParams {
    pub position: f64,
}

/// Params of `volume`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VolumeParams {
    /// 0 (silent) to 1; values outside are clamped.
    pub volume: f32,
}

/// Params of `shuffle`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ShuffleParams {
    pub on: bool,
}

/// Params of `repeat`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RepeatParams {
    pub repeat: Repeat,
}
