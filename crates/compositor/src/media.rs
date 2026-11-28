//! The media control: what the top bar shows of what audiod is playing, and what
//! the bar's buttons and the keyboard's media keys ask audiod to do.
//!
//! audiod owns playback; the compositor is one more remote control, like the
//! Music app and `osctl audio`. It follows audiod's `watch` stream (see
//! [`crate::bus`]) and sends plain requests back. The control is trusted chrome:
//! the compositor draws it, so no app can cover or imitate it, and the media
//! keys reach audiod wherever the focus is, with the Music app closed.

use protocol::audio::{AudioStatus, PlayState, method};
use serde_json::{Value, json};

use crate::input::MediaKey;

/// The app that clicking what's playing opens.
pub const MUSIC_APP: &str = "org.idealos.music";
/// How much a volume key changes the volume.
pub const VOLUME_STEP: f64 = 0.05;
/// The volume Mute goes back to when nothing was muted from.
const UNMUTED: f64 = 0.5;
/// Most characters of "title — artist" the bar shows.
const MAX_TEXT: usize = 48;

/// What the bar shows: whether music is playing, and what.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub playing: bool,
    pub text: String,
}

/// What the bar shows for `status`, or `None` to hide the control (no audiod,
/// or nothing queued).
pub fn shown(status: Option<&AudioStatus>) -> Option<Shown> {
    let s = status?;
    let item = s.current.and_then(|i| s.queue.get(i as usize))?;
    let text = match &item.artist {
        Some(artist) => format!("{} — {artist}", item.title),
        None => item.title.clone(),
    };
    Some(Shown { playing: s.state == PlayState::Playing, text: clip(&text, MAX_TEXT) })
}

/// At most `max` characters of `s`, with an ellipsis if cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>().trim_end())
    }
}

/// What the bar's buttons ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Previous,
    Toggle,
    Next,
    /// Open the Music app (what's playing was clicked).
    Open,
}

/// The audiod request (method and params) for a bar button, if it is one.
pub fn request_for(action: Action) -> Option<(&'static str, Value)> {
    Some(match action {
        Action::Previous => (method::PREVIOUS, json!({})),
        Action::Toggle => (method::TOGGLE, json!({})),
        Action::Next => (method::NEXT, json!({})),
        Action::Open => return None,
    })
}

/// The audiod request for media key `key`, given what audiod last said. The
/// volume keys need to know the volume, so without a status they do nothing.
/// `muted_from` is the volume Mute took away, kept for the next Mute to give back.
pub fn request_for_key(
    key: MediaKey,
    status: Option<&AudioStatus>,
    muted_from: &mut Option<f64>,
) -> Option<(&'static str, Value)> {
    let volume = |v: f64| Some((method::VOLUME, json!({"volume": v})));
    Some(match key {
        MediaKey::PlayPause => (method::TOGGLE, json!({})),
        MediaKey::Pause => (method::PAUSE, json!({})),
        MediaKey::Stop => (method::STOP, json!({})),
        MediaKey::Next => (method::NEXT, json!({})),
        MediaKey::Previous => (method::PREVIOUS, json!({})),
        MediaKey::VolumeUp => return volume(step(status?.volume.into(), 1.0)),
        MediaKey::VolumeDown => return volume(step(status?.volume.into(), -1.0)),
        MediaKey::Mute => {
            // audiod's volume is an f32: back to two places, as a person would set it.
            let now = (f64::from(status?.volume) * 100.0).round() / 100.0;
            if now > 0.0 {
                *muted_from = Some(now);
                return volume(0.0);
            }
            return volume(muted_from.take().unwrap_or(UNMUTED));
        }
    })
}

/// The volume one step from `v` (up for `dir` 1, down for -1), on the steps'
/// grid, so a volume set by a slider joins it, and within 0 to 1.
fn step(v: f64, dir: f64) -> f64 {
    let steps = (v / VOLUME_STEP).round() + dir;
    ((steps * VOLUME_STEP).clamp(0.0, 1.0) * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::audio::{QueueItem, Repeat};

    fn status(state: PlayState, current: Option<u32>, volume: f32) -> AudioStatus {
        let item = |title: &str, artist: Option<&str>| QueueItem {
            id: title.to_lowercase(),
            title: title.into(),
            artist: artist.map(Into::into),
            album: None,
            duration: None,
            artwork: None,
        };
        AudioStatus {
            state,
            queue: vec![item("Shadowboxer", Some("Fiona Apple")), item("Untitled", None)],
            current,
            position: 0.0,
            at: 0,
            duration: None,
            volume,
            shuffle: false,
            repeat: Repeat::Off,
            output: None,
            error: None,
        }
    }

    #[test]
    fn the_bar_shows_whats_playing_or_hides() {
        let playing = status(PlayState::Playing, Some(0), 1.0);
        assert_eq!(
            shown(Some(&playing)),
            Some(Shown { playing: true, text: "Shadowboxer — Fiona Apple".into() })
        );
        let paused = status(PlayState::Paused, Some(1), 1.0);
        assert_eq!(shown(Some(&paused)), Some(Shown { playing: false, text: "Untitled".into() }));
        // Stopped with a queue still shows the track Play would start.
        let stopped = status(PlayState::Stopped, Some(0), 1.0);
        assert_eq!(shown(Some(&stopped)).map(|s| s.playing), Some(false));
        let empty = AudioStatus { queue: vec![], ..status(PlayState::Stopped, None, 1.0) };
        assert_eq!(shown(Some(&empty)), None, "nothing queued");
        assert_eq!(shown(None), None, "no audiod");
    }

    #[test]
    fn long_titles_are_cut_short() {
        let mut s = status(PlayState::Playing, Some(0), 1.0);
        s.queue[0].title = "a".repeat(80);
        let text = shown(Some(&s)).unwrap().text;
        assert_eq!(text.chars().count(), MAX_TEXT);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn bar_buttons_ask_audiod() {
        assert_eq!(request_for(Action::Toggle).unwrap().0, "toggle");
        assert_eq!(request_for(Action::Next).unwrap().0, "next");
        assert_eq!(request_for(Action::Previous).unwrap().0, "previous");
        assert_eq!(request_for(Action::Open), None, "opening Music isn't audiod's");
    }

    #[test]
    fn media_keys_ask_audiod() {
        let s = status(PlayState::Playing, Some(0), 0.5);
        let mut muted = None;
        let mut key = |k| request_for_key(k, Some(&s), &mut muted);
        assert_eq!(key(MediaKey::PlayPause), Some(("toggle", json!({}))));
        assert_eq!(key(MediaKey::Pause), Some(("pause", json!({}))));
        assert_eq!(key(MediaKey::Stop), Some(("stop", json!({}))));
        assert_eq!(key(MediaKey::Next), Some(("next", json!({}))));
        assert_eq!(key(MediaKey::Previous), Some(("previous", json!({}))));
        // Play/Pause works before audiod has said anything; the volume keys can't.
        assert_eq!(request_for_key(MediaKey::PlayPause, None, &mut None).unwrap().0, "toggle");
        assert_eq!(request_for_key(MediaKey::VolumeUp, None, &mut None), None);
    }

    #[test]
    fn volume_keys_step_and_stay_in_range() {
        let vol = |v: f32, k| {
            let (m, p) =
                request_for_key(k, Some(&status(PlayState::Playing, Some(0), v)), &mut None).unwrap();
            assert_eq!(m, "volume");
            p["volume"].as_f64().unwrap()
        };
        assert_eq!(vol(0.5, MediaKey::VolumeUp), 0.55);
        assert_eq!(vol(0.5, MediaKey::VolumeDown), 0.45);
        assert_eq!(vol(0.52, MediaKey::VolumeUp), 0.55, "a slider's volume joins the steps");
        assert_eq!(vol(1.0, MediaKey::VolumeUp), 1.0);
        assert_eq!(vol(0.0, MediaKey::VolumeDown), 0.0);
    }

    #[test]
    fn mute_gives_the_volume_back() {
        let mut muted = None;
        let at = |v: f32| status(PlayState::Playing, Some(0), v);
        let (_, p) = request_for_key(MediaKey::Mute, Some(&at(0.7)), &mut muted).unwrap();
        assert_eq!(p["volume"], 0.0);
        let (_, p) = request_for_key(MediaKey::Mute, Some(&at(0.0)), &mut muted).unwrap();
        assert_eq!(p["volume"], 0.7);
        // Muted some other way (a slider at 0): Mute turns it up to something audible.
        let (_, p) = request_for_key(MediaKey::Mute, Some(&at(0.0)), &mut None).unwrap();
        assert_eq!(p["volume"], 0.5);
    }
}
