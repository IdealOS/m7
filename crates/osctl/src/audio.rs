//! `osctl audio ...`: drive audiod. Tracks are `Track` document ids, or a whole
//! album (`--album NAME`, in disc and track order).

use std::io::Write;

use bus_client::Client;
use protocol::audio::{AudioStatus, OutputsResult, PlayState, SERVICE, method};
use protocol::db::DocsResult;
use serde_json::{Value, json};

/// Which tracks.
#[derive(Debug, PartialEq)]
pub enum Tracks {
    Ids(Vec<String>),
    /// An album's tracks, in order; `artist` picks one of several albums with the name.
    Album {
        album: String,
        artist: Option<String>,
    },
}

/// What to do.
#[derive(Debug, PartialEq)]
pub enum Audio {
    Status,
    /// The sound outputs there are, the one in use marked.
    Outputs,
    /// Print each change until interrupted.
    Watch,
    Play {
        tracks: Tracks,
        start: u32,
    },
    Enqueue {
        tracks: Tracks,
        next: bool,
    },
    /// A method that takes no params: pause, resume, toggle, stop, next,
    /// previous, clear.
    Simple(&'static str),
    Jump(u32),
    Remove(u32),
    Seek(f64),
    Volume(f32),
    Shuffle(bool),
    Repeat(String),
}

/// `osctl audio SUB ARGS...`.
pub fn parse(sub: &str, positional: &[String], flags: &[(String, String)]) -> Result<Audio, String> {
    let flag = |name: &str| flags.iter().find(|(f, _)| f == name).map(|(_, v)| v.clone());
    let allowed = |names: &[&str]| match flags.iter().find(|(f, _)| !names.contains(&f.as_str())) {
        Some((f, _)) => Err(format!("audio {sub} has no --{f}")),
        None => Ok(()),
    };
    let tracks = || -> Result<Tracks, String> {
        match (flag("album"), positional.is_empty()) {
            (Some(album), true) => Ok(Tracks::Album { album, artist: flag("artist") }),
            (None, false) => Ok(Tracks::Ids(positional.to_vec())),
            (Some(_), false) => Err("give track ids or --album, not both".into()),
            (None, true) => Err(format!("audio {sub} needs track ids or --album NAME")),
        }
    };
    let index = |what: &str| -> Result<u32, String> {
        match positional {
            [n] => n.parse().map_err(|_| format!("{what} takes a queue position (0 is the first), not {n}")),
            _ => Err(format!("audio {what} takes one queue position")),
        }
    };
    let none = || if positional.is_empty() { Ok(()) } else { Err(format!("audio {sub} takes no arguments")) };
    let simple = [
        method::PAUSE,
        method::RESUME,
        method::TOGGLE,
        method::STOP,
        method::NEXT,
        method::PREVIOUS,
        method::CLEAR,
    ];
    match sub {
        "status" | "watch" | "outputs" => {
            allowed(&[])?;
            none()?;
            Ok(match sub {
                "status" => Audio::Status,
                "watch" => Audio::Watch,
                _ => Audio::Outputs,
            })
        }
        "play" => {
            allowed(&["album", "artist", "start"])?;
            let start = match flag("start") {
                Some(s) => s.parse().map_err(|_| format!("--start takes a number, not {s}"))?,
                None => 0,
            };
            Ok(Audio::Play { tracks: tracks()?, start })
        }
        "enqueue" => {
            allowed(&["album", "artist", "next"])?;
            let next = matches!(flag("next").as_deref(), Some("yes" | "true"));
            Ok(Audio::Enqueue { tracks: tracks()?, next })
        }
        "jump" | "remove" => {
            allowed(&[])?;
            let i = index(sub)?;
            Ok(if sub == "jump" { Audio::Jump(i) } else { Audio::Remove(i) })
        }
        "seek" | "volume" => {
            allowed(&[])?;
            let [v] = positional else { return Err(format!("audio {sub} takes one number")) };
            let n: f64 = v.parse().map_err(|_| format!("audio {sub} takes a number, not {v}"))?;
            Ok(if sub == "seek" { Audio::Seek(n) } else { Audio::Volume(n as f32) })
        }
        "shuffle" => {
            allowed(&[])?;
            match positional {
                [v] if v == "on" => Ok(Audio::Shuffle(true)),
                [v] if v == "off" => Ok(Audio::Shuffle(false)),
                _ => Err("audio shuffle takes on or off".into()),
            }
        }
        "repeat" => {
            allowed(&[])?;
            match positional {
                [v] if ["off", "all", "one"].contains(&v.as_str()) => Ok(Audio::Repeat(v.clone())),
                _ => Err("audio repeat takes off, all or one".into()),
            }
        }
        s => match simple.iter().find(|m| **m == s) {
            Some(m) => {
                allowed(&[])?;
                none()?;
                Ok(Audio::Simple(m))
            }
            None => Err(format!("unknown audio subcommand {s}")),
        },
    }
}

async fn call(bus: &Client, m: &str, params: Value) -> Result<AudioStatus, String> {
    let v = bus.request(SERVICE, m, params).await.map_err(|e| e.to_string())?;
    serde_json::from_value(v).map_err(|e| format!("audiod answered oddly: {e}"))
}

/// The ids of `tracks`.
async fn ids(bus: &Client, tracks: Tracks) -> Result<Vec<String>, String> {
    let (album, artist) = match tracks {
        Tracks::Ids(ids) => return Ok(ids),
        Tracks::Album { album, artist } => (album, artist),
    };
    let mut filter = json!({"album": album});
    if let Some(a) = &artist {
        filter["$or"] = json!([{"albumArtist": a}, {"artist": a}]);
    }
    let q = json!({
        "type": "Track",
        "where": filter,
        "orderBy": [["discNumber", "asc"], ["trackNumber", "asc"], ["title", "asc"]],
        "limit": protocol::audio::MAX_QUEUE,
    });
    let v = bus
        .request(protocol::db::SERVICE, protocol::db::method::QUERY, q)
        .await
        .map_err(|e| e.to_string())?;
    let found: DocsResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    if found.docs.is_empty() {
        return Err(format!("no tracks on an album called {album:?}"));
    }
    Ok(found.docs.into_iter().map(|d| d.id).collect())
}

pub async fn run(bus: &Client, what: Audio, out: &mut impl Write) -> Result<(), String> {
    let w = |e: std::io::Error| e.to_string();
    let status = match what {
        Audio::Status => call(bus, method::STATUS, Value::Null).await?,
        Audio::Outputs => {
            let v = bus.request(SERVICE, method::OUTPUTS, Value::Null).await.map_err(|e| e.to_string())?;
            let r: OutputsResult =
                serde_json::from_value(v).map_err(|e| format!("audiod answered oddly: {e}"))?;
            for o in &r.outputs {
                let mark = if r.current.as_ref() == Some(&o.id) { ">" } else { " " };
                writeln!(out, "{mark} {}  ({})", o.name, o.id).map_err(w)?;
            }
            if r.outputs.is_empty() {
                writeln!(out, "no sound outputs").map_err(w)?;
            }
            return Ok(());
        }
        Audio::Watch => {
            let mut stream =
                bus.request_stream(SERVICE, method::WATCH, Value::Null).await.map_err(|e| e.to_string())?;
            while let Some(part) = stream.next().await {
                let s: AudioStatus = serde_json::from_value(part.map_err(|e| e.to_string())?)
                    .map_err(|e| format!("audiod answered oddly: {e}"))?;
                writeln!(out, "{}", line(&s)).map_err(w)?;
                out.flush().map_err(w)?;
            }
            return Ok(());
        }
        Audio::Play { tracks, start } => {
            let tracks = ids(bus, tracks).await?;
            call(bus, method::PLAY, json!({"tracks": tracks, "start": start})).await?
        }
        Audio::Enqueue { tracks, next } => {
            let tracks = ids(bus, tracks).await?;
            call(bus, method::ENQUEUE, json!({"tracks": tracks, "next": next})).await?
        }
        Audio::Simple(m) => call(bus, m, Value::Null).await?,
        Audio::Jump(i) => call(bus, method::JUMP, json!({"index": i})).await?,
        Audio::Remove(i) => call(bus, method::REMOVE, json!({"index": i})).await?,
        Audio::Seek(s) => call(bus, method::SEEK, json!({"position": s})).await?,
        Audio::Volume(v) => call(bus, method::VOLUME, json!({"volume": v})).await?,
        Audio::Shuffle(on) => call(bus, method::SHUFFLE, json!({"on": on})).await?,
        Audio::Repeat(r) => call(bus, method::REPEAT, json!({"repeat": r})).await?,
    };
    writeln!(out, "{}", line(&status)).map_err(w)?;
    for (i, q) in status.queue.iter().enumerate() {
        let mark = if status.current == Some(i as u32) { ">" } else { " " };
        let by = q.artist.as_deref().map(|a| format!(" - {a}")).unwrap_or_default();
        let len = q.duration.map(|d| format!("  {}", clock(d))).unwrap_or_default();
        writeln!(out, "{mark} {i:>3}  {}{by}{len}", q.title).map_err(w)?;
    }
    Ok(())
}

/// One line about what's playing.
pub fn line(s: &AudioStatus) -> String {
    let state = match s.state {
        PlayState::Playing => "playing",
        PlayState::Paused => "paused",
        PlayState::Stopped => "stopped",
    };
    let mut l = state.to_owned();
    if let Some(item) = s.current.and_then(|i| s.queue.get(i as usize)) {
        l += &format!(" {}/{}: {}", s.current.unwrap_or(0) + 1, s.queue.len(), item.title);
        if let Some(a) = &item.artist {
            l += &format!(" - {a}");
        }
        l += &format!("  {}", clock(s.position));
        if let Some(d) = s.duration {
            l += &format!(" / {}", clock(d));
        }
    } else {
        l += "  (nothing queued)";
    }
    l += &format!("  volume {:.0}%", s.volume * 100.0);
    if s.shuffle {
        l += "  shuffle";
    }
    if s.repeat != protocol::audio::Repeat::Off {
        l += &format!(
            "  repeat {}",
            serde_json::to_value(s.repeat).unwrap_or_default().as_str().unwrap_or("?")
        );
    }
    l += &format!("  [{}]", s.output.as_deref().unwrap_or("silent"));
    if let Some(e) = &s.error {
        l += &format!("\n  last problem: {e}");
    }
    l
}

/// Seconds as m:ss.
fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::audio::{QueueItem, Repeat};

    fn strings(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    fn flags(f: &[(&str, &str)]) -> Vec<(String, String)> {
        f.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn audio_commands_parse() {
        assert_eq!(parse("status", &[], &[]), Ok(Audio::Status));
        assert_eq!(parse("outputs", &[], &[]), Ok(Audio::Outputs));
        assert_eq!(
            parse("play", &strings(&["a", "b"]), &[]),
            Ok(Audio::Play { tracks: Tracks::Ids(strings(&["a", "b"])), start: 0 })
        );
        assert_eq!(
            parse("play", &[], &flags(&[("album", "Shepherd Moons"), ("start", "3")])),
            Ok(Audio::Play {
                tracks: Tracks::Album { album: "Shepherd Moons".into(), artist: None },
                start: 3
            })
        );
        assert_eq!(
            parse("enqueue", &strings(&["a"]), &flags(&[("next", "yes")])),
            Ok(Audio::Enqueue { tracks: Tracks::Ids(strings(&["a"])), next: true })
        );
        assert_eq!(parse("next", &[], &[]), Ok(Audio::Simple("next")));
        assert_eq!(parse("jump", &strings(&["4"]), &[]), Ok(Audio::Jump(4)));
        assert_eq!(parse("seek", &strings(&["61.5"]), &[]), Ok(Audio::Seek(61.5)));
        assert_eq!(parse("volume", &strings(&["0.5"]), &[]), Ok(Audio::Volume(0.5)));
        assert_eq!(parse("shuffle", &strings(&["on"]), &[]), Ok(Audio::Shuffle(true)));
        assert_eq!(parse("repeat", &strings(&["one"]), &[]), Ok(Audio::Repeat("one".into())));
        assert!(parse("play", &[], &[]).is_err());
        assert!(parse("play", &strings(&["a"]), &flags(&[("album", "x")])).is_err());
        assert!(parse("pause", &strings(&["x"]), &[]).is_err());
        assert!(parse("pause", &[], &flags(&[("album", "x")])).is_err());
        assert!(parse("shuffle", &strings(&["maybe"]), &[]).is_err());
        assert!(parse("repeat", &strings(&["twice"]), &[]).is_err());
        assert!(parse("jump", &strings(&["-1"]), &[]).is_err());
        assert!(parse("dance", &[], &[]).is_err());
    }

    #[test]
    fn the_status_line_says_whats_playing() {
        let s = AudioStatus {
            state: PlayState::Playing,
            queue: vec![QueueItem {
                id: "t".into(),
                title: "Caribbean Blue".into(),
                artist: Some("Enya".into()),
                album: None,
                duration: Some(239.0),
                artwork: None,
            }],
            current: Some(0),
            position: 83.4,
            at: 0,
            duration: Some(239.0),
            volume: 0.8,
            shuffle: true,
            repeat: Repeat::All,
            output: None,
            error: None,
        };
        assert_eq!(
            line(&s),
            "playing 1/1: Caribbean Blue - Enya  1:23 / 3:59  volume 80%  shuffle  repeat all  [silent]"
        );
    }
}
