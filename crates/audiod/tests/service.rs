//! audiod with dbd on a real broker, playing silently (and faster than real
//! time): play an album through, count its plays, and drive the queue.

use std::io::{Seek, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bus_client::{CallError, Client};
use protocol::audio::{AudioStatus, PlayState, Repeat, code};
use serde_json::{Value, json};
use tokio::net::UnixListener;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!("audiod-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A broker, dbd and audiod playing silently at `speed` times real time.
async fn start(dir: &Path, speed: f64) -> Client {
    let socket = dir.join("bus.sock");
    tokio::spawn(busd::serve(UnixListener::bind(&socket).unwrap(), Box::new(busd::AllowAll), None));
    let path = dir.join("idealos.db");
    let db_bus = Client::connect(&socket).await.unwrap();
    db_bus.claim(protocol::db::SERVICE).await.unwrap();
    tokio::spawn(dbd::serve(db_bus, db::Store::open(&path).unwrap(), dbd::Config::beside(&path)));
    let audio_bus = Client::connect(&socket).await.unwrap();
    audio_bus.claim(protocol::audio::SERVICE).await.unwrap();
    let config = audiod::Config { silent: true, speed, ..audiod::Config::default() };
    let engine = audiod::Engine::start(&config);
    tokio::spawn(audiod::serve(audio_bus, engine));
    Client::connect(&socket).await.unwrap()
}

/// `secs` of a tone as 8 kHz mono WAV bytes.
fn wav(secs: f64) -> Vec<u8> {
    let frames = (8000.0 * secs) as u32;
    let mut w = b"RIFF".to_vec();
    w.extend((36 + frames * 2).to_le_bytes());
    w.extend(b"WAVEfmt ");
    w.extend(16u32.to_le_bytes());
    w.extend([1, 0, 1, 0]);
    w.extend(8000u32.to_le_bytes());
    w.extend(16000u32.to_le_bytes());
    w.extend([2, 0, 16, 0]);
    w.extend(b"data");
    w.extend((frames * 2).to_le_bytes());
    for n in 0..frames {
        w.extend((((n as f64 * 0.3).sin() * 5000.0) as i16).to_le_bytes());
    }
    w
}

/// Store `bytes` as a blob: its hash.
async fn blob(c: &Client, dir: &Path, bytes: &[u8]) -> String {
    let path = dir.join(format!("blob-{}", bytes.len()));
    let mut f =
        std::fs::File::options().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    f.write_all(bytes).unwrap();
    f.rewind().unwrap();
    let reply = c
        .request_with(protocol::db::SERVICE, "put_blob", Value::Null, vec![OwnedFd::from(f)], None)
        .await
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    reply.result["hash"].as_str().unwrap().to_owned()
}

/// A Track of `secs` seconds called `title`: its id.
async fn track(c: &Client, dir: &Path, title: &str, secs: f64) -> String {
    let hash = blob(c, dir, &wav(secs)).await;
    let data = json!({"title": title, "artist": "The Testers", "album": "Tones", "duration": secs, "audio": {"$blob": hash}});
    let doc =
        c.request(protocol::db::SERVICE, "create", json!({"type": "Track", "data": data})).await.unwrap();
    doc["id"].as_str().unwrap().to_owned()
}

async fn call(c: &Client, method: &str, params: Value) -> Result<AudioStatus, CallError> {
    let v = c.request(protocol::audio::SERVICE, method, params).await?;
    Ok(serde_json::from_value(v).unwrap())
}

async fn status(c: &Client) -> AudioStatus {
    call(c, "status", Value::Null).await.unwrap()
}

/// Wait until `ok` holds for the status.
async fn until(c: &Client, what: &str, ok: impl Fn(&AudioStatus) -> bool) -> AudioStatus {
    for _ in 0..1000 {
        let s = status(c).await;
        if ok(&s) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never {what}: {:?}", status(c).await);
}

fn code_of(e: CallError) -> String {
    match e {
        CallError::Bus(e) => e.code,
        CallError::Disconnected => "disconnected".into(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn plays_an_album_through_and_counts_each_track() {
    let dir = TempDir::new("album");
    let c = start(&dir.0, 5.0).await;
    let ids = [
        track(&c, &dir.0, "One", 1.0).await,
        track(&c, &dir.0, "Two", 1.0).await,
        track(&c, &dir.0, "Three", 1.0).await,
    ];
    let mut watch = c.request_stream(protocol::audio::SERVICE, "watch", Value::Null).await.unwrap();
    let first: AudioStatus = serde_json::from_value(watch.next().await.unwrap().unwrap()).unwrap();
    assert_eq!(first.state, PlayState::Stopped);
    assert!(first.queue.is_empty());

    let s = call(&c, "play", json!({"tracks": ids})).await.unwrap();
    assert_eq!(s.state, PlayState::Playing);
    assert_eq!(s.current, Some(0));
    assert_eq!(s.queue.iter().map(|q| q.title.as_str()).collect::<Vec<_>>(), ["One", "Two", "Three"]);
    assert_eq!(s.queue[0].artist.as_deref(), Some("The Testers"));
    assert_eq!(s.output, None, "silent");

    // Watchers see each track start, then the end; at 5x, 3 s of music takes 0.6 s
    // (each track lasting longer than the service's tick).
    let mut seen = Vec::new();
    loop {
        let s: AudioStatus = serde_json::from_value(watch.next().await.unwrap().unwrap()).unwrap();
        if s.state == PlayState::Playing && seen.last() != Some(&s.current) {
            seen.push(s.current);
        }
        if s.state == PlayState::Stopped {
            assert_eq!(s.current, Some(0), "ready to play again from the top");
            assert_eq!(s.position, 0.0);
            break;
        }
    }
    assert_eq!(seen, [Some(0), Some(1), Some(2)]);

    // Each was heard to its end once.
    for id in &ids {
        let doc = c.request(protocol::db::SERVICE, "get", json!({"id": id})).await;
        let doc = until_counted(&c, id, doc.unwrap()).await;
        assert_eq!(doc["data"]["playCount"], 1, "{id}");
        assert!(doc["data"]["lastPlayed"].as_i64().unwrap() > 0);
    }
}

/// The document `id` once its play has been counted (it's written just after).
async fn until_counted(c: &Client, id: &str, mut doc: Value) -> Value {
    for _ in 0..200 {
        if doc["data"]["playCount"].is_number() {
            return doc;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        doc = c.request(protocol::db::SERVICE, "get", json!({"id": id})).await.unwrap();
    }
    doc
}

#[tokio::test(flavor = "current_thread")]
async fn pause_seek_skip_and_change_the_queue() {
    let dir = TempDir::new("controls");
    let c = start(&dir.0, 1.0).await;
    let a = track(&c, &dir.0, "A", 30.0).await;
    let b = track(&c, &dir.0, "B", 30.0).await;
    let d = track(&c, &dir.0, "D", 30.0).await;

    call(&c, "play", json!({"tracks": [a, b], "start": 1})).await.unwrap();
    let s = until(&c, "playing B", |s| s.current == Some(1) && s.position > 0.05).await;
    assert_eq!(s.duration, Some(30.0));

    let s = call(&c, "pause", Value::Null).await.unwrap();
    assert_eq!(s.state, PlayState::Paused);
    let held = s.position;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(status(&c).await.position, held, "paused, the position holds");

    let s = call(&c, "seek", json!({"position": 20.0})).await.unwrap();
    assert_eq!(s.state, PlayState::Paused, "seeking keeps it paused");
    assert_eq!(s.position, 20.0);
    let s = call(&c, "resume", Value::Null).await.unwrap();
    assert_eq!(s.state, PlayState::Playing);
    until(&c, "moving on from 20 s", |s| s.position > 20.05).await;

    // Well into a track, previous goes back to its start; near the start, to the one before.
    call(&c, "previous", Value::Null).await.unwrap();
    let s = until(&c, "back at the start of B", |s| s.current == Some(1) && s.position < 1.0).await;
    assert_eq!(s.state, PlayState::Playing);
    let s = call(&c, "previous", Value::Null).await.unwrap();
    assert_eq!(s.current, Some(0));
    let s = call(&c, "previous", Value::Null).await.unwrap();
    assert_eq!(s.current, Some(0), "nothing before the first, without repeat");

    // Play next, then skip to it.
    let s = call(&c, "enqueue", json!({"tracks": [d], "next": true})).await.unwrap();
    assert_eq!(s.queue.iter().map(|q| q.title.as_str()).collect::<Vec<_>>(), ["A", "D", "B"]);
    let s = call(&c, "next", Value::Null).await.unwrap();
    assert_eq!(s.current, Some(1));
    until(&c, "playing D", |s| s.current == Some(1) && s.position > 0.05).await;

    // Removing the track that's playing moves on to the next.
    let s = call(&c, "remove", json!({"index": 1})).await.unwrap();
    assert_eq!(s.queue.iter().map(|q| q.title.as_str()).collect::<Vec<_>>(), ["A", "B"]);
    assert_eq!(s.current, Some(1));
    assert_eq!(s.state, PlayState::Playing);

    // Past the last track: stop, without repeat; wrap around with it.
    let s = call(&c, "next", Value::Null).await.unwrap();
    assert_eq!((s.state, s.current), (PlayState::Stopped, Some(0)));
    call(&c, "repeat", json!({"repeat": "all"})).await.unwrap();
    call(&c, "jump", json!({"index": 1})).await.unwrap();
    let s = call(&c, "next", Value::Null).await.unwrap();
    assert_eq!((s.state, s.current, s.repeat), (PlayState::Playing, Some(0), Repeat::All));

    let s = call(&c, "volume", json!({"volume": 0.25})).await.unwrap();
    assert_eq!(s.volume, 0.25);
    let s = call(&c, "toggle", Value::Null).await.unwrap();
    assert_eq!(s.state, PlayState::Paused);
    let s = call(&c, "stop", Value::Null).await.unwrap();
    assert_eq!((s.state, s.position), (PlayState::Stopped, 0.0));
    let s = call(&c, "clear", Value::Null).await.unwrap();
    assert!(s.queue.is_empty());
    assert_eq!(code_of(call(&c, "next", Value::Null).await.unwrap_err()), code::EMPTY);
}

#[tokio::test(flavor = "current_thread")]
async fn shuffles_and_puts_the_order_back() {
    let dir = TempDir::new("shuffle");
    let c = start(&dir.0, 1.0).await;
    let mut ids = Vec::new();
    for i in 0..12 {
        ids.push(track(&c, &dir.0, &format!("T{i}"), 30.0).await);
    }
    let order = |s: &AudioStatus| s.queue.iter().map(|q| q.id.clone()).collect::<Vec<_>>();
    let s = call(&c, "play", json!({"tracks": ids})).await.unwrap();
    assert_eq!(order(&s), ids);
    let s = call(&c, "shuffle", json!({"on": true})).await.unwrap();
    assert!(s.shuffle);
    assert_eq!(s.queue[0].id, ids[0], "the playing track stays first");
    assert_ne!(order(&s), ids);
    let s = call(&c, "shuffle", json!({"on": false})).await.unwrap();
    assert_eq!(order(&s), ids);
    assert_eq!(s.current, Some(0));
    // Shuffle on, a new album starts with the track picked, the rest shuffled.
    call(&c, "shuffle", json!({"on": true})).await.unwrap();
    let s = call(&c, "play", json!({"tracks": ids, "start": 5})).await.unwrap();
    assert_eq!(s.queue[0].id, ids[5]);
    assert_eq!(s.current, Some(0));
    assert_eq!(s.queue.len(), 12);
}

#[tokio::test(flavor = "current_thread")]
async fn skips_what_it_cannot_play_and_refuses_what_is_not_a_track() {
    let dir = TempDir::new("bad");
    let c = start(&dir.0, 10.0).await;
    let junk_hash = blob(&c, &dir.0, &[9u8; 5000]).await;
    let junk = c
        .request(
            protocol::db::SERVICE,
            "create",
            json!({"type": "Track", "data": {"title": "Junk", "audio": {"$blob": junk_hash}}}),
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let good = track(&c, &dir.0, "Good", 0.5).await;
    call(&c, "play", json!({"tracks": [junk, good]})).await.unwrap();
    let s =
        until(&c, "playing the good one", |s| s.current == Some(1) && s.state == PlayState::Playing).await;
    assert!(s.error.as_deref().is_some_and(|e| e.starts_with("Junk: ")), "{:?}", s.error);
    until(&c, "done", |s| s.state == PlayState::Stopped).await;

    // A queue of nothing but junk stops rather than spinning.
    call(&c, "repeat", json!({"repeat": "all"})).await.unwrap();
    call(&c, "play", json!({"tracks": [junk]})).await.unwrap();
    until(&c, "giving up", |s| s.state == PlayState::Stopped).await;

    let note = c
        .request(protocol::db::SERVICE, "create", json!({"type": "Note", "data": {"title": "x"}}))
        .await
        .unwrap();
    let e = call(&c, "play", json!({"tracks": [note["id"]]})).await.unwrap_err();
    assert_eq!(code_of(e), code::NOT_FOUND);
    let e = call(&c, "play", json!({"tracks": ["no-such-id"]})).await.unwrap_err();
    assert_eq!(code_of(e), code::NOT_FOUND);
    let e = call(&c, "play", json!({"tracks": []})).await.unwrap_err();
    assert_eq!(code_of(e), protocol::bus::code::BAD_REQUEST);
    let e = call(&c, "play", json!({"tracks": [good], "start": 3})).await.unwrap_err();
    assert_eq!(code_of(e), code::NOT_FOUND);
    let e = call(&c, "jump", json!({"index": 99})).await.unwrap_err();
    assert_eq!(code_of(e), code::NOT_FOUND);
}
