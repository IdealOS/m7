//! The `audio` service: plays `Track` documents, and owns the queue.
//!
//! Three parts, so a slow request never interrupts the sound:
//!
//! - this service, on the bus: requests, the queue, what's playing;
//! - the decoder thread ([`player`]): reads each track's blob (an open file db
//!   hands over), decodes it ([`decode`]), converts it to the output's rate
//!   ([`resample`]) and fills a ring buffer;
//! - the output ([`output`]): the sound device's callback mixes from the ring.
//!
//! The decoder says at which output frame each track begins; the mixer counts the
//! frames it plays. Comparing the two, the service knows which track is audible
//! and where in it, and that's what `status` and `watch` report. When a track is
//! heard to its end, its `playCount` and `lastPlayed` are updated.

pub mod decode;
pub mod gain;
pub mod output;
pub mod player;
pub mod queue;
pub mod resample;

use std::collections::VecDeque;
use std::fs::File;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bus_client::{CallError, Client, Incoming};
use protocol::audio::{
    AudioStatus, EnqueueParams, IndexParams, LEVELING_KEY, Leveling, MAX_QUEUE, OUTPUT_KEY, OutputsResult,
    PlayParams, PlayState, QueueItem, Repeat, RepeatParams, SeekParams, ShuffleParams, VolumeParams, code,
    method,
};
use protocol::bus::code as bus_code;
use protocol::db::{self, DocsResult};
use protocol::{BusError, Sender};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

use gain::ReplayGain;
use output::{Output, Shared};
use player::{Cmd, Event, Load};
use queue::{Key, Queue, Source, Wrap};

/// How often the service checks which track is audible.
pub const TICK: Duration = Duration::from_millis(100);
/// Within this many seconds of a track's start, `previous` goes to the track
/// before; after it, back to the start of this one.
pub const RESTART_AFTER: f64 = 3.0;

/// How audiod plays.
#[derive(Debug, Clone)]
pub struct Config {
    /// The sound device: the one with this id, or any whose name contains it;
    /// `None` for the default. The `audio.output` setting, once read, decides.
    pub device: Option<String>,
    /// Play to nowhere, keeping time (no sound device; for tests and machines
    /// without sound).
    pub silent: bool,
    /// How fast silent output runs: 1 is real time.
    pub speed: f64,
    pub volume: f32,
}

impl Default for Config {
    fn default() -> Config {
        Config { device: None, silent: false, speed: 1.0, volume: 0.8 }
    }
}

/// The audio path: the output, the decoder thread, and how to reach them.
pub struct Engine {
    output: Output,
    shared: Arc<Shared>,
    cmds: std::sync::mpsc::Sender<Cmd>,
    events: UnboundedReceiver<Event>,
    config: Config,
}

impl Engine {
    /// Open the output (the default one if the one asked for isn't there, and
    /// silent if there's no sound device at all) and start the decoder thread.
    pub fn start(config: &Config) -> Engine {
        let shared = Shared::new(config.volume.clamp(0.0, 1.0));
        let ring = Arc::new(Mutex::new(None));
        let make = |shared: Arc<Shared>, ring: Arc<Mutex<Option<rtrb::Producer<f32>>>>| {
            move |rate: u32| {
                let (producer, mixer) = output::ring(rate, shared);
                *ring.lock().unwrap() = Some(producer);
                mixer
            }
        };
        let output = if config.silent {
            Output::null(output::DEFAULT_RATE, config.speed, make(shared.clone(), ring.clone()))
        } else {
            let chosen = match &config.device {
                Some(d) => Output::device(Some(d), make(shared.clone(), ring.clone())).map_err(|e| {
                    log::warn!("audiod: {e}; playing to the default output");
                }),
                None => Err(()),
            };
            match chosen.or_else(|()| Output::device(None, make(shared.clone(), ring.clone()))) {
                Ok(o) => o,
                Err(e) => {
                    log::warn!("audiod: {e}; playing silently");
                    Output::null(output::DEFAULT_RATE, 1.0, make(shared.clone(), ring.clone()))
                }
            }
        };
        let producer = ring.lock().unwrap().take().expect("the output made the ring");
        let (cmds, cmd_rx) = std::sync::mpsc::channel();
        let (event_tx, events) = tokio::sync::mpsc::unbounded_channel();
        // The thread ends when `cmds` is dropped.
        player::spawn(output.rate, shared.clone(), producer, cmd_rx, event_tx);
        log::info!(
            "audiod: playing to {} at {} Hz",
            output.name.as_deref().unwrap_or("nowhere (silent)"),
            output.rate
        );
        Engine { output, shared, cmds, events, config: config.clone() }
    }
}

/// Where a track begins in the stream.
#[derive(Debug, Clone, PartialEq)]
struct Mark {
    key: Key,
    frame: u64,
    start: f64,
    duration: Option<f64>,
}

struct Service {
    bus: Client,
    shared: Arc<Shared>,
    cmds: std::sync::mpsc::Sender<Cmd>,
    rate: u32,
    output: Option<String>,
    output_id: Option<String>,
    /// How audiod was started, for starting again on another output.
    config: Config,
    /// The `audio.output` setting, once read (`Some(None)`: there's none).
    chosen: Option<Option<String>>,
    leveling: Leveling,
    queue: Queue,
    state: PlayState,
    shuffle: bool,
    repeat: Repeat,
    error: Option<String>,
    /// The track playing, paused, or to play next.
    current: Option<Key>,
    /// Bumped whenever the stream is dropped, so stale events are ignored.
    epoch: u64,
    /// The track heard now, and tracks decoded but not heard yet.
    mark: Option<Mark>,
    pending: VecDeque<Mark>,
    /// Where the stream ends, once nothing follows.
    finished_at: Option<u64>,
    /// The position when not playing.
    hold: f64,
    /// What the decoder was last told follows a track.
    told: Option<(Key, Option<Key>)>,
    /// Tracks that failed in a row; past the queue's length, playing stops.
    failures: usize,
    watchers: Vec<u64>,
    dirty: bool,
}

/// Serve `engine` on `bus`, which must already have claimed `audio`.
pub async fn serve(bus: Client, engine: Engine) -> Result<(), String> {
    let Some(mut requests) = bus.take_requests() else { return Ok(()) };
    let Engine { output, shared, cmds, mut events, config } = engine;
    let (settings_tx, mut settings) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(follow_settings(bus.clone(), settings_tx));
    let seed =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
    let mut s = Service {
        bus: bus.clone(),
        shared,
        cmds,
        rate: output.rate,
        output: output.name.clone(),
        output_id: output.id.clone(),
        config,
        chosen: None,
        leveling: Leveling::default(),
        queue: Queue::new(seed),
        state: PlayState::Stopped,
        shuffle: false,
        repeat: Repeat::Off,
        error: None,
        current: None,
        epoch: 0,
        mark: None,
        pending: VecDeque::new(),
        finished_at: None,
        hold: 0.0,
        told: None,
        failures: 0,
        watchers: Vec::new(),
        dirty: false,
    };
    let mut output = Some(output);
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            incoming = requests.next() => match incoming {
                Some(Incoming::Request { id, from, method, params, stream, .. }) => {
                    s.request(id, from, &method, params, stream).await;
                }
                Some(Incoming::Cancel { id }) => s.watchers.retain(|w| *w != id),
                None => break,
            },
            Some(event) = events.recv() => s.event(event).await,
            Some(change) = settings.recv() => match change {
                Setting::Leveling(l) => s.set_leveling(l).await,
                Setting::Output(id) => {
                    // With no setting at first, stay on the output audiod started with.
                    let started = s.chosen.is_none() && id.is_none();
                    if s.chosen.as_ref() != Some(&id) {
                        s.chosen = Some(id.clone());
                        if !started {
                            s.switch_output(&mut output, &mut events, id).await;
                        }
                    }
                }
            },
            _ = tick.tick() => s.sync(),
        }
        if s.dirty {
            s.dirty = false;
            if !s.watchers.is_empty() {
                let v = to_value(s.status());
                for &w in &s.watchers {
                    let _ = s.bus.stream(w, v.clone()).await;
                }
            }
        }
    }
    drop(output);
    Ok(())
}

impl Service {
    async fn request(&mut self, id: u64, from: Option<Sender>, method: &str, params: Value, stream: bool) {
        if method == method::OUTPUTS {
            let outputs = tokio::task::spawn_blocking(output::outputs).await.unwrap_or_default();
            let current = self.output_id.clone().filter(|c| outputs.iter().any(|o| &o.id == c));
            let _ = self.bus.respond(id, Ok(to_value(OutputsResult { outputs, current }))).await;
            return;
        }
        if method == method::WATCH {
            if stream {
                let _ = self.bus.stream(id, to_value(self.status())).await;
                self.watchers.push(id);
            } else {
                let e = bad_request("watch must be sent as a streamed request");
                let _ = self.bus.respond(id, Err(e)).await;
            }
            return;
        }
        let answer = match self.call(from.as_ref(), method, params).await {
            Ok(()) => Ok(to_value(self.status())),
            Err(e) => Err(e),
        };
        let _ = self.bus.respond(id, answer).await;
    }

    async fn call(&mut self, from: Option<&Sender>, m: &str, params: Value) -> Result<(), BusError> {
        match m {
            method::STATUS => return Ok(()),
            method::PLAY => {
                let p: PlayParams = parse(params)?;
                let items = self.tracks(from, &p.tracks).await?;
                if p.start as usize >= items.len() {
                    return Err(fail(
                        code::NOT_FOUND,
                        format!("no track {} in a list of {}", p.start, items.len()),
                    ));
                }
                let keys = self.queue.replace(items);
                let first = keys[p.start as usize];
                if self.shuffle {
                    self.queue.set_shuffle(true, None);
                    self.queue.move_to_front(first);
                }
                self.error = None;
                self.failures = 0;
                self.play_from(first, 0.0).await;
            }
            method::ENQUEUE => {
                let p: EnqueueParams = parse(params)?;
                let items = self.tracks(from, &p.tracks).await?;
                if self.queue.len() + items.len() > MAX_QUEUE {
                    return Err(fail(code::TOO_MANY, format!("the queue holds at most {MAX_QUEUE} tracks")));
                }
                let keys = self.queue.enqueue(items, if p.next { self.current } else { None });
                if self.current.is_none() {
                    self.current = keys.first().copied();
                }
                self.recheck_next().await;
            }
            method::PAUSE => self.pause(),
            method::RESUME => self.resume().await?,
            method::TOGGLE => match self.state {
                PlayState::Playing => self.pause(),
                _ => self.resume().await?,
            },
            method::STOP => {
                self.stop();
                self.hold = 0.0;
            }
            method::NEXT => {
                let cur = self.current.ok_or_else(empty)?;
                let wrap = if self.repeat == Repeat::Off { Wrap::No } else { Wrap::Yes };
                match self.queue.step(cur, 1, wrap) {
                    Some(k) => self.go_to(k, 0.0).await,
                    None => self.run_out(),
                }
            }
            method::PREVIOUS => {
                let cur = self.current.ok_or_else(empty)?;
                let wrap = if self.repeat == Repeat::Off { Wrap::No } else { Wrap::Yes };
                let target =
                    if self.position() > RESTART_AFTER { None } else { self.queue.step(cur, -1, wrap) };
                self.go_to(target.unwrap_or(cur), 0.0).await;
            }
            method::JUMP => {
                let p: IndexParams = parse(params)?;
                let k = self.queue.key_at(p.index as usize).ok_or_else(|| no_index(p.index))?;
                self.error = None;
                self.failures = 0;
                self.play_from(k, 0.0).await;
            }
            method::REMOVE => {
                let p: IndexParams = parse(params)?;
                let k = self.queue.key_at(p.index as usize).ok_or_else(|| no_index(p.index))?;
                self.remove(k).await;
            }
            method::CLEAR => {
                self.stop();
                self.queue.clear();
                self.current = None;
                self.hold = 0.0;
            }
            method::SEEK => {
                let p: SeekParams = parse(params)?;
                let cur = self.current.ok_or_else(empty)?;
                let mut to = if p.position.is_finite() { p.position.max(0.0) } else { 0.0 };
                if let Some(d) = self.duration() {
                    to = to.min(d);
                }
                if self.state == PlayState::Stopped {
                    self.hold = to;
                } else {
                    self.start(cur, to).await;
                }
            }
            method::VOLUME => {
                let p: VolumeParams = parse(params)?;
                let v = if p.volume.is_finite() { p.volume } else { 0.0 };
                self.shared.set_volume(v);
            }
            method::SHUFFLE => {
                let p: ShuffleParams = parse(params)?;
                self.shuffle = p.on;
                self.queue.set_shuffle(p.on, self.current);
                self.recheck_next().await;
            }
            method::REPEAT => {
                let p: RepeatParams = parse(params)?;
                self.repeat = p.repeat;
                self.recheck_next().await;
            }
            other => return Err(bad_request(format!("audio has no method {other:?}"))),
        }
        self.dirty = true;
        Ok(())
    }

    /// The queue entries for track ids, read as the caller.
    async fn tracks(
        &self,
        from: Option<&Sender>,
        ids: &[String],
    ) -> Result<Vec<(QueueItem, Source)>, BusError> {
        if ids.is_empty() {
            return Err(bad_request("no tracks given"));
        }
        if ids.len() > MAX_QUEUE {
            return Err(fail(code::TOO_MANY, format!("the queue holds at most {MAX_QUEUE} tracks")));
        }
        let mut unique: Vec<&String> = ids.iter().collect();
        unique.sort();
        unique.dedup();
        let mut q = json!({"type": "Track", "where": {"$id": unique}, "limit": unique.len()});
        if let Some(Sender::App { app, .. }) = from {
            q["as"] = json!(app);
        }
        let found: DocsResult = self.db(db::method::QUERY, q).await?;
        ids.iter()
            .map(|id| {
                let doc = found
                    .docs
                    .iter()
                    .find(|d| &d.id == id)
                    .ok_or_else(|| fail(code::NOT_FOUND, format!("{id} isn't a Track you may read")))?;
                let text = |k: &str| doc.data.get(k).and_then(Value::as_str).map(str::to_owned);
                let blob = |k: &str| {
                    doc.data.get(k).and_then(|v| v.get("$blob")).and_then(Value::as_str).map(str::to_owned)
                };
                let audio =
                    blob("audio").ok_or_else(|| fail(code::NOT_FOUND, format!("{id} has no audio")))?;
                let item = QueueItem {
                    id: id.clone(),
                    title: text("title").unwrap_or_else(|| "Untitled".into()),
                    artist: text("artist"),
                    album: text("album"),
                    duration: doc.data.get("duration").and_then(Value::as_f64),
                    artwork: blob("artwork"),
                };
                Ok((item, Source { audio, gain: ReplayGain::from_doc(doc.data.get("replayGain")) }))
            })
            .collect()
    }

    /// What to scale track `key`'s samples by, as levelling is set now.
    fn gain(&self, key: Key) -> f32 {
        self.queue.get(key).map_or(1.0, |e| e.gain.factor(self.leveling))
    }

    /// Level loudness another way, from where the music is now.
    async fn set_leveling(&mut self, leveling: Leveling) {
        if self.leveling == leveling {
            return;
        }
        log::info!("audiod: levelling {leveling:?}");
        self.leveling = leveling;
        if self.state != PlayState::Stopped {
            self.restart_here().await;
        }
    }

    /// Play to another output (`device`, or the one audiod started with), going
    /// on from where the music was. The old one is closed first: a sound card
    /// plays one stream at a time.
    async fn switch_output(
        &mut self,
        output: &mut Option<Output>,
        events: &mut UnboundedReceiver<Event>,
        device: Option<String>,
    ) {
        let (state, at, volume) = (self.state, self.position(), self.shared.volume());
        self.stop();
        *output = None;
        let config =
            Config { device: device.or_else(|| self.config.device.clone()), volume, ..self.config.clone() };
        // Opening a device takes a moment; audiod answers nothing meanwhile.
        let engine = Engine::start(&config);
        self.rate = engine.output.rate;
        self.output = engine.output.name.clone();
        self.output_id = engine.output.id.clone();
        self.shared = engine.shared;
        self.cmds = engine.cmds;
        *events = engine.events;
        *output = Some(engine.output);
        self.hold = at;
        self.dirty = true;
        let Some(cur) = self.current else { return };
        match state {
            PlayState::Playing => self.play_from(cur, at).await,
            PlayState::Paused => {
                self.shared.paused.store(true, Ordering::Relaxed);
                self.state = PlayState::Paused;
                self.start(cur, at).await;
            }
            PlayState::Stopped => {}
        }
    }

    /// Play `key` from `at`, playing (not paused).
    async fn play_from(&mut self, key: Key, at: f64) {
        self.shared.paused.store(false, Ordering::Relaxed);
        self.state = PlayState::Playing;
        self.start(key, at).await;
    }

    /// Move to `key`: start it if playing or paused (staying paused), else just
    /// make it current.
    async fn go_to(&mut self, key: Key, at: f64) {
        if self.state == PlayState::Stopped {
            self.current = Some(key);
            self.hold = at;
        } else {
            self.start(key, at).await;
        }
    }

    /// Drop the stream and decode `key` from `at`. A track whose audio can't be
    /// had is skipped.
    async fn start(&mut self, mut key: Key, at: f64) {
        let mut at = at;
        loop {
            self.epoch += 1;
            self.pending.clear();
            self.mark = None;
            self.finished_at = None;
            self.told = None;
            self.current = Some(key);
            self.hold = at;
            self.dirty = true;
            match self.open_audio(key).await {
                Ok(file) => {
                    let _ = self.cmds.send(Cmd::Start {
                        epoch: self.epoch,
                        load: Load { key, file, gain: self.gain(key) },
                        at,
                    });
                    return;
                }
                Err(e) => {
                    self.failed(key, &e);
                    let next = self.queue.after(key, self.skip_repeat());
                    match next {
                        Some(n) if self.failures <= self.queue.len() => {
                            key = n;
                            at = 0.0;
                        }
                        _ => {
                            self.run_out();
                            return;
                        }
                    }
                }
            }
        }
    }

    fn pause(&mut self) {
        if self.state == PlayState::Playing {
            self.hold = self.position();
            self.shared.paused.store(true, Ordering::Relaxed);
            self.state = PlayState::Paused;
        }
    }

    async fn resume(&mut self) -> Result<(), BusError> {
        match self.state {
            PlayState::Playing => {}
            PlayState::Paused => {
                self.shared.paused.store(false, Ordering::Relaxed);
                self.state = PlayState::Playing;
            }
            PlayState::Stopped => {
                let k = self.current.or_else(|| self.queue.key_at(0)).ok_or_else(empty)?;
                self.failures = 0;
                self.play_from(k, self.hold).await;
            }
        }
        Ok(())
    }

    /// Stop playing, keeping the queue and the current track.
    fn stop(&mut self) {
        self.epoch += 1;
        let _ = self.cmds.send(Cmd::Stop { epoch: self.epoch });
        self.pending.clear();
        self.mark = None;
        self.finished_at = None;
        self.told = None;
        self.shared.paused.store(false, Ordering::Relaxed);
        self.state = PlayState::Stopped;
        self.dirty = true;
    }

    /// The queue ran out: stop, ready to play it again from the top.
    fn run_out(&mut self) {
        self.stop();
        self.current = self.queue.key_at(0);
        self.hold = 0.0;
    }

    async fn remove(&mut self, key: Key) {
        if Some(key) == self.current {
            let next = self.queue.after(key, self.skip_repeat());
            self.queue.remove(key);
            match (self.state, next) {
                (PlayState::Stopped, n) => {
                    self.current = n.or_else(|| self.queue.key_at(0));
                    self.hold = 0.0;
                }
                (_, Some(n)) => self.start(n, 0.0).await,
                (_, None) => self.run_out(),
            }
        } else if self.pending.iter().any(|m| m.key == key) {
            // Already decoded after the current track: decode again without it.
            self.queue.remove(key);
            self.restart_here().await;
        } else {
            self.queue.remove(key);
            self.recheck_next().await;
        }
    }

    /// The queue changed: if what follows the last decoded track isn't what the
    /// decoder was told, tell it again (or, if it has already started on it,
    /// decode again from where the music is).
    async fn recheck_next(&mut self) {
        let Some((after, told)) = self.told else { return };
        if self.queue.after(after, self.repeat) == told {
            return;
        }
        if told.is_some_and(|n| self.pending.iter().any(|m| m.key == n)) {
            self.restart_here().await;
        } else {
            self.want(after).await;
        }
    }

    async fn restart_here(&mut self) {
        if let Some(cur) = self.current {
            let at = self.position();
            self.start(cur, at).await;
        }
    }

    /// The repeat mode for moving on past a track that failed or was removed:
    /// repeating one would play it again.
    fn skip_repeat(&self) -> Repeat {
        if self.repeat == Repeat::One { Repeat::All } else { self.repeat }
    }

    fn failed(&mut self, key: Key, e: &str) {
        let title = self.queue.get(key).map_or("a track", |e| e.item.title.as_str());
        log::warn!("audiod: {title}: {e}");
        self.error = Some(format!("{title}: {e}"));
        self.failures += 1;
        self.dirty = true;
    }

    async fn event(&mut self, event: Event) {
        let epoch = match &event {
            Event::Began { epoch, .. }
            | Event::Want { epoch, .. }
            | Event::Failed { epoch, .. }
            | Event::Finished { epoch, .. } => *epoch,
        };
        if epoch != self.epoch {
            return;
        }
        match event {
            Event::Began { key, frame, start, duration, .. } => {
                self.failures = 0;
                self.pending.push_back(Mark { key, frame, start, duration });
                self.sync();
            }
            Event::Want { after, .. } => self.want(after).await,
            Event::Failed { key, error, .. } => {
                self.failed(key, &error);
                if self.failures > self.queue.len() {
                    self.run_out();
                }
            }
            Event::Finished { frame, .. } => {
                self.finished_at = Some(frame);
                self.sync();
            }
        }
    }

    /// Tell the decoder what follows `after`.
    async fn want(&mut self, after: Key) {
        let mut next = self.queue.after(after, self.repeat);
        loop {
            let Some(k) = next else {
                let _ = self.cmds.send(Cmd::Last { epoch: self.epoch, after });
                self.told = Some((after, None));
                return;
            };
            match self.open_audio(k).await {
                Ok(file) => {
                    let _ = self.cmds.send(Cmd::Next {
                        epoch: self.epoch,
                        after,
                        load: Load { key: k, file, gain: self.gain(k) },
                    });
                    self.told = Some((after, Some(k)));
                    return;
                }
                Err(e) => {
                    self.failed(k, &e);
                    next = if self.failures > self.queue.len() {
                        None
                    } else {
                        self.queue.after(k, self.skip_repeat())
                    };
                }
            }
        }
    }

    /// Catch up with what's audible: tracks heard to their end, and the end of
    /// the queue.
    fn sync(&mut self) {
        if self.state == PlayState::Stopped {
            return;
        }
        let heard = self.shared.since_flush();
        while self.pending.front().is_some_and(|m| m.frame <= heard) {
            let m = self.pending.pop_front().expect("checked");
            if let Some(prev) = self.mark.replace(m.clone()) {
                self.played_through(prev.key);
            }
            self.current = Some(m.key);
            self.dirty = true;
        }
        if self.finished_at.is_some_and(|end| heard >= end) {
            if let Some(m) = self.mark.take() {
                self.played_through(m.key);
            }
            self.run_out();
        }
    }

    /// Track `key` was heard to its end: count it.
    fn played_through(&self, key: Key) {
        let Some(entry) = self.queue.get(key) else { return };
        let (bus, id) = (self.bus.clone(), entry.item.id.clone());
        tokio::spawn(async move {
            let count = match bus.request(db::SERVICE, db::method::GET, json!({"id": id})).await {
                Ok(doc) => doc["data"]["playCount"].as_u64().unwrap_or(0),
                Err(e) => {
                    log::warn!("audiod: counting a play of {id}: {}", message(e));
                    return;
                }
            };
            let patch = json!({"id": id, "patch": {"playCount": count + 1, "lastPlayed": now_ms()}});
            if let Err(e) = bus.request(db::SERVICE, db::method::UPDATE, patch).await {
                log::warn!("audiod: counting a play of {id}: {}", message(e));
            }
        });
    }

    /// Where the current track is, in seconds.
    fn position(&self) -> f64 {
        // Paused or stopped, it's where it was left; playing, it's counted from
        // where the track began in the stream.
        let at = match (&self.mark, self.state) {
            (Some(m), PlayState::Playing) => {
                m.start + self.shared.since_flush().saturating_sub(m.frame) as f64 / self.rate as f64
            }
            _ => self.hold,
        };
        match self.duration() {
            Some(d) => at.min(d),
            None => at,
        }
    }

    fn duration(&self) -> Option<f64> {
        let cur = self.current?;
        match &self.mark {
            Some(m) if m.key == cur && m.duration.is_some() => m.duration,
            _ => self.queue.get(cur).and_then(|e| e.item.duration),
        }
    }

    fn status(&self) -> AudioStatus {
        AudioStatus {
            state: self.state,
            queue: self.queue.entries().iter().map(|e| e.item.clone()).collect(),
            current: self.current.and_then(|k| self.queue.index(k)).map(|i| i as u32),
            position: self.position(),
            at: now_ms(),
            duration: self.duration(),
            volume: self.shared.volume(),
            shuffle: self.shuffle,
            repeat: self.repeat,
            output: self.output.clone(),
            error: self.error.clone(),
        }
    }

    /// Track `key`'s audio, open, from db.
    async fn open_audio(&self, key: Key) -> Result<File, String> {
        let hash = self.queue.get(key).map(|e| e.audio.clone()).ok_or("it's no longer queued")?;
        let reply = self
            .bus
            .request_with(db::SERVICE, db::method::GET_BLOB, json!({"hash": hash}), Vec::new(), None)
            .await
            .map_err(|e| format!("its audio: {}", message(e)))?;
        let fd = reply.fds.into_iter().next().ok_or("db sent no audio")?;
        Ok(File::from(fd))
    }

    async fn db<T: DeserializeOwned>(&self, m: &str, params: Value) -> Result<T, BusError> {
        let v = self.bus.request(db::SERVICE, m, params).await.map_err(|e| match e {
            CallError::Bus(e) => e,
            CallError::Disconnected => fail(bus_code::NO_SUCH_SERVICE, "the bus closed the connection"),
        })?;
        serde_json::from_value(v).map_err(|e| fail(bus_code::BAD_REQUEST, format!("db answered oddly: {e}")))
    }
}

/// A change to one of the settings audiod follows.
#[derive(Debug, Clone, PartialEq)]
enum Setting {
    Output(Option<String>),
    Leveling(Leveling),
}

/// Follow the `audio.output` and `audio.leveling` settings with a live query,
/// sending each change (the output also when there's no setting, so the first
/// report says so). If dbd isn't there (yet, or any more), try again a little
/// later.
async fn follow_settings(bus: Client, changes: tokio::sync::mpsc::UnboundedSender<Setting>) {
    use protocol::db::{Doc, WatchEvent};
    let query = json!({"type": "Setting", "where": {"key": {"$in": [OUTPUT_KEY, LEVELING_KEY]}}});
    loop {
        let mut stream = match bus.request_stream(db::SERVICE, "watch", query.clone()).await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        let mut docs: std::collections::HashMap<String, Doc> = std::collections::HashMap::new();
        let mut last: (Option<Option<String>>, Option<Leveling>) = (None, None);
        while let Some(Ok(part)) = stream.next().await {
            match serde_json::from_value::<WatchEvent>(part) {
                Ok(WatchEvent::Initial { docs: all }) => {
                    docs = all.into_iter().map(|d| (d.id.clone(), d)).collect()
                }
                Ok(WatchEvent::Diff { added, changed, removed, .. }) => {
                    for id in removed {
                        docs.remove(&id);
                    }
                    for d in added.into_iter().chain(changed) {
                        docs.insert(d.id.clone(), d);
                    }
                }
                Err(_) => continue,
            }
            // For each key, the most recently changed document decides.
            let value = |key: &str| {
                docs.values()
                    .filter(|d| d.data.get("key") == Some(&json!(key)))
                    .max_by_key(|d| d.updated)
                    .and_then(|d| d.data.get("value").cloned())
            };
            let output =
                value(OUTPUT_KEY).and_then(|v| v.as_str().map(str::to_owned)).filter(|s| !s.is_empty());
            let leveling = value(LEVELING_KEY).and_then(|v| serde_json::from_value::<Leveling>(v).ok());
            if last.0.as_ref() != Some(&output) {
                last.0 = Some(output.clone());
                if changes.send(Setting::Output(output)).is_err() {
                    return;
                }
            }
            let leveling = leveling.unwrap_or_default();
            if last.1 != Some(leveling) {
                last.1 = Some(leveling);
                if changes.send(Setting::Leveling(leveling)).is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, BusError> {
    serde_json::from_value(params).map_err(|e| bad_request(format!("bad params: {e}")))
}

fn to_value(v: impl serde::Serialize) -> Value {
    serde_json::to_value(v).expect("results serialize")
}

fn fail(code: &str, message: impl Into<String>) -> BusError {
    BusError { code: code.into(), message: message.into() }
}

fn bad_request(message: impl Into<String>) -> BusError {
    fail(bus_code::BAD_REQUEST, message)
}

fn empty() -> BusError {
    fail(code::EMPTY, "nothing is queued")
}

fn no_index(i: u32) -> BusError {
    fail(code::NOT_FOUND, format!("the queue has no entry {i}"))
}

fn message(e: CallError) -> String {
    match e {
        CallError::Bus(e) => e.message,
        CallError::Disconnected => "the bus closed the connection".into(),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}
