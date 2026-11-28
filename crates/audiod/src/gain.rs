//! Levelling: playing tracks at an even loudness from their ReplayGain tags.
//!
//! importd keeps a file's ReplayGain as a `Track`'s `replayGain`: how many dB to
//! turn the track (or its whole album) to reach the standard loudness, and its
//! loudest sample. audiod scales each track's samples by that as it decodes
//! them, so a change of gain falls exactly where one track gives way to the next.

use protocol::audio::Leveling;
use serde_json::Value;

/// Most a track is turned up, whatever its tags say: past this, quiet passages'
/// noise comes up with them.
const MAX_BOOST_DB: f32 = 12.0;

/// A track's ReplayGain, as its document has it. Gains in dB; peaks where 1 is
/// full scale.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReplayGain {
    pub track_gain: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_gain: Option<f32>,
    pub album_peak: Option<f32>,
}

impl ReplayGain {
    /// From a `Track` document's `replayGain` (anything else gives none).
    pub fn from_doc(v: Option<&Value>) -> ReplayGain {
        let n = |key: &str| {
            v.and_then(|v| v.get(key)).and_then(Value::as_f64).map(|x| x as f32).filter(|x| x.is_finite())
        };
        let peak = |key: &str| n(key).filter(|p| *p > 0.0);
        ReplayGain {
            track_gain: n("trackGain"),
            track_peak: peak("trackPeak"),
            album_gain: n("albumGain"),
            album_peak: peak("albumPeak"),
        }
    }

    /// What to multiply the track's samples by under `leveling`: 1 when off or
    /// untagged. Album levelling uses the track's gain when there's no album
    /// gain, and the other way round. Never so much that the loudest sample
    /// would clip, nor more than [`MAX_BOOST_DB`] up.
    pub fn factor(&self, leveling: Leveling) -> f32 {
        let track = self.track_gain.map(|g| (g, self.track_peak));
        let album = self.album_gain.map(|g| (g, self.album_peak));
        let chosen = match leveling {
            Leveling::Off => None,
            Leveling::Track => track.or(album),
            Leveling::Album => album.or(track),
        };
        let Some((db, peak)) = chosen else { return 1.0 };
        let mut f = 10f32.powf(db.min(MAX_BOOST_DB) / 20.0);
        if let Some(p) = peak {
            f = f.min(1.0 / p);
        }
        f
    }
}

/// Scale `samples` by `factor`, into `out` (cleared first).
pub fn apply(samples: &[f32], factor: f32, out: &mut Vec<f32>) {
    out.clear();
    out.extend(samples.iter().map(|s| s * factor));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn replay_gain_comes_from_the_document_and_ignores_nonsense() {
        let rg = ReplayGain::from_doc(Some(
            &json!({"trackGain": -6.0, "trackPeak": 0.5, "albumGain": "loud", "albumPeak": -1}),
        ));
        assert_eq!(
            rg,
            ReplayGain { track_gain: Some(-6.0), track_peak: Some(0.5), album_gain: None, album_peak: None }
        );
        assert_eq!(ReplayGain::from_doc(None), ReplayGain::default());
        assert_eq!(ReplayGain::from_doc(Some(&json!("x"))), ReplayGain::default());
    }

    #[test]
    fn levelling_turns_each_track_or_album_to_the_same_loudness() {
        let rg =
            ReplayGain { track_gain: Some(-6.0), track_peak: None, album_gain: Some(-3.0), album_peak: None };
        assert_eq!(rg.factor(Leveling::Off), 1.0);
        assert!(close(rg.factor(Leveling::Track), 0.501_19), "-6 dB is about half");
        assert!(close(rg.factor(Leveling::Album), 0.707_95));
        // Each falls back to the other.
        let track_only = ReplayGain { track_gain: Some(-6.0), ..ReplayGain::default() };
        assert!(close(track_only.factor(Leveling::Album), 0.501_19));
        let album_only = ReplayGain { album_gain: Some(-3.0), ..ReplayGain::default() };
        assert!(close(album_only.factor(Leveling::Track), 0.707_95));
        assert_eq!(ReplayGain::default().factor(Leveling::Track), 1.0, "untagged plays as it is");
    }

    #[test]
    fn levelling_never_clips_or_boosts_too_far() {
        // +6 dB would double it, but its peak is already at 0.8: only to 1.25.
        let rg = ReplayGain { track_gain: Some(6.0), track_peak: Some(0.8), ..ReplayGain::default() };
        assert!(close(rg.factor(Leveling::Track), 1.25));
        let huge = ReplayGain { track_gain: Some(40.0), ..ReplayGain::default() };
        assert!(close(huge.factor(Leveling::Track), 10f32.powf(12.0 / 20.0)));
        let mut out = vec![9.0];
        apply(&[0.5, -0.25], 2.0, &mut out);
        assert_eq!(out, [1.0, -0.5]);
    }
}
