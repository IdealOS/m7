//! What a music file says about itself, read with lofty: its tags (ID3, MP4,
//! Vorbis comments, RIFF INFO and the rest, whichever the file has), how long it
//! plays, and its cover art.

use std::io::BufReader;

use lofty::file::FileType;
use lofty::picture::PictureType;
use lofty::prelude::*;
use lofty::probe::Probe;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// A picture embedded in a music file: its bytes and MIME type.
#[derive(Debug, Clone, PartialEq)]
pub struct Artwork {
    pub bytes: Vec<u8>,
    pub mime: Option<String>,
}

/// What a music file's tags say. Anything missing is left out of its `Track`.
/// `import-reader` sends it to importd without the artwork, which follows apart.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub year: Option<u16>,
    /// Seconds.
    pub duration: Option<f64>,
    /// ReplayGain: how many dB to turn the track (or its whole album) up or down
    /// to play at the standard loudness, and its loudest sample (1 is full scale).
    pub track_gain: Option<f64>,
    pub track_peak: Option<f64>,
    pub album_gain: Option<f64>,
    pub album_peak: Option<f64>,
    #[serde(skip)]
    pub artwork: Option<Artwork>,
}

/// Read the tags of `file`, whose extension is `ext` (how lofty knows the format,
/// as it would from a path; by its first bytes when the extension says nothing).
/// A file lofty can't make sense of is an error; the caller can still import it,
/// by its name. Runs in `import-reader`, never in importd itself.
pub fn read(file: &mut std::fs::File, ext: &str) -> Result<Tags, String> {
    let probe = Probe::new(BufReader::new(file));
    let probe = match FileType::from_ext(ext) {
        Some(t) => probe.set_file_type(t),
        None => probe.guess_file_type().map_err(|e| e.to_string())?,
    };
    let file = probe.read().map_err(|e| e.to_string())?;
    let seconds = file.properties().duration().as_secs_f64();
    let mut tags = Tags { duration: (seconds > 0.0).then_some(seconds), ..Tags::default() };
    let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) else { return Ok(tags) };
    let text =
        |s: Option<std::borrow::Cow<'_, str>>| s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    tags.title = text(tag.title());
    tags.artist = text(tag.artist());
    tags.album = text(tag.album());
    tags.genre = text(tag.genre());
    tags.album_artist = text(tag.get_string(ItemKey::AlbumArtist).map(Into::into));
    tags.track = tag.track();
    tags.disc = tag.disk();
    tags.year = tag.date().map(|d| d.year).filter(|&y| y > 0);
    let number = |key: ItemKey| tag.get_string(key).and_then(replay_gain_number);
    tags.track_gain = number(ItemKey::ReplayGainTrackGain);
    tags.track_peak = number(ItemKey::ReplayGainTrackPeak);
    tags.album_gain = number(ItemKey::ReplayGainAlbumGain);
    tags.album_peak = number(ItemKey::ReplayGainAlbumPeak);
    let picture = tag.get_picture_type(PictureType::CoverFront).or_else(|| tag.pictures().first());
    tags.artwork = picture
        .filter(|p| !p.data().is_empty())
        .map(|p| Artwork { bytes: p.data().to_vec(), mime: p.mime_type().map(|m| m.as_str().to_owned()) });
    Ok(tags)
}

/// A ReplayGain tag's number: "-6.54 dB" for a gain, "0.988" for a peak.
fn replay_gain_number(text: &str) -> Option<f64> {
    let t = text.trim();
    let t =
        t.strip_suffix("dB").or_else(|| t.strip_suffix("db")).or_else(|| t.strip_suffix("DB")).unwrap_or(t);
    t.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// A `Track` document's data from `tags`, titled `name` when the file has no title.
pub fn track(tags: &Tags, name: &str, audio: &str, artwork: Option<&str>) -> Map<String, Value> {
    let mut t = Map::new();
    t.insert("title".into(), json!(tags.title.clone().unwrap_or_else(|| name.to_owned())));
    let mut put = |key: &str, v: Value| {
        if !v.is_null() {
            t.insert(key.into(), v);
        }
    };
    put("artist", json!(tags.artist));
    put("album", json!(tags.album));
    put("albumArtist", json!(tags.album_artist));
    put("genre", json!(tags.genre));
    put("trackNumber", json!(tags.track));
    put("discNumber", json!(tags.disc));
    put("year", json!(tags.year));
    put("duration", json!(tags.duration.map(|d| (d * 1000.0).round() / 1000.0)));
    let mut gain = Map::new();
    for (key, v) in [
        ("trackGain", tags.track_gain),
        ("trackPeak", tags.track_peak),
        ("albumGain", tags.album_gain),
        ("albumPeak", tags.album_peak),
    ] {
        if let Some(v) = v {
            gain.insert(key.into(), json!(v));
        }
    }
    if !gain.is_empty() {
        t.insert("replayGain".into(), Value::Object(gain));
    }
    t.insert("audio".into(), json!({"$blob": audio}));
    if let Some(hash) = artwork {
        t.insert("artwork".into(), json!({"$blob": hash}));
    }
    t
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture};
    use lofty::tag::{Tag, TagType};
    use std::path::Path;

    fn read_path(path: &Path) -> Result<Tags, String> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_owned();
        read(&mut std::fs::File::open(path).map_err(|e| e.to_string())?, &ext)
    }

    /// A second of silent 8 kHz mono WAV at `path`, tagged (ID3v2) as given.
    pub fn tagged_wav(path: &Path, title: Option<&str>, picture: Option<&[u8]>) {
        let samples = 8000u32;
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend((36 + samples * 2).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes()); // PCM
        wav.extend(1u16.to_le_bytes()); // mono
        wav.extend(8000u32.to_le_bytes());
        wav.extend(16000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend((samples * 2).to_le_bytes());
        wav.resize(wav.len() + samples as usize * 2, 0);
        std::fs::write(path, wav).unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        if let Some(t) = title {
            tag.set_title(t.into());
            tag.set_artist("The Testers".into());
            tag.set_album("Silence".into());
            tag.insert_text(ItemKey::AlbumArtist, "Various".into());
            tag.set_track(3);
            tag.set_disk(1);
            tag.set_genre("Ambient".into());
            tag.insert_text(ItemKey::ReplayGainTrackGain, "-6.54 dB".into());
            tag.insert_text(ItemKey::ReplayGainTrackPeak, "0.988525".into());
            tag.insert_text(ItemKey::ReplayGainAlbumGain, "+1.2 dB".into());
        }
        if let Some(bytes) = picture {
            tag.push_picture(
                Picture::unchecked(bytes.to_vec())
                    .pic_type(PictureType::CoverFront)
                    .mime_type(MimeType::Png)
                    .build(),
            );
        }
        tag.save_to_path(path, WriteOptions::default()).unwrap();
    }

    #[test]
    fn tags_duration_and_artwork_come_from_the_file() {
        let dir = std::env::temp_dir().join(format!("importd-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.wav");
        tagged_wav(&path, Some("Quiet"), Some(b"\x89PNG fake cover"));
        let tags = read_path(&path).unwrap();
        assert_eq!(tags.title.as_deref(), Some("Quiet"));
        assert_eq!(tags.artist.as_deref(), Some("The Testers"));
        assert_eq!(tags.album_artist.as_deref(), Some("Various"));
        assert_eq!((tags.track, tags.disc), (Some(3), Some(1)));
        assert!((tags.duration.unwrap() - 1.0).abs() < 0.01, "{:?}", tags.duration);
        let art = tags.artwork.clone().unwrap();
        assert_eq!(
            (art.bytes.as_slice(), art.mime.as_deref()),
            (&b"\x89PNG fake cover"[..], Some("image/png"))
        );

        let data = track(&tags, "song", "aa", Some("bb"));
        assert_eq!(data["title"], "Quiet");
        assert_eq!(data["trackNumber"], 3);
        assert_eq!(data["audio"], json!({"$blob": "aa"}));
        assert_eq!(data["artwork"], json!({"$blob": "bb"}));
        assert!(!data.contains_key("year"), "nothing missing is written");
        assert_eq!(
            data["replayGain"],
            json!({"trackGain": -6.54, "trackPeak": 0.988525, "albumGain": 1.2}),
            "ReplayGain as numbers; no album peak was tagged"
        );

        // No title: the file's name.
        let bare = dir.join("bare.wav");
        tagged_wav(&bare, None, None);
        let tags = read_path(&bare).unwrap();
        assert_eq!(track(&tags, "bare", "aa", None)["title"], "bare");
        assert!(!track(&tags, "bare", "aa", None).contains_key("replayGain"));
        let junk = dir.join("junk.mp3");
        std::fs::write(&junk, b"not music at all").unwrap();
        assert!(read_path(&junk).is_err());
        // No extension: lofty goes by the first bytes.
        assert_eq!(
            read(&mut std::fs::File::open(&path).unwrap(), "").unwrap().title.as_deref(),
            Some("Quiet")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
