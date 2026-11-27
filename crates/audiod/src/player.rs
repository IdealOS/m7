//! The decoder thread: decodes tracks, converts them to the output's rate, and
//! keeps the ring full.
//!
//! The service tells it what to play ([`Cmd`]) and hears back where each track
//! begins in the stream ([`Event`]), counted in output frames since the last
//! flush. Comparing that with the frames the mixer has played tells the service
//! which track is audible, without the audio path ever waiting on it.
//!
//! Gapless: as soon as a track begins, the thread asks what follows it
//! ([`Event::Want`]), and decodes that straight on from the last frame of this
//! one, through the same resampler, so the stream has no seam.

use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use crate::decode::Track;
use crate::output::Shared;
use crate::queue::Key;
use crate::resample::Resampler;

/// A track to play: its queue key and its open audio.
pub struct Load {
    pub key: Key,
    pub file: File,
}

pub enum Cmd {
    /// Drop what's buffered and play `load` from `at` seconds.
    Start { epoch: u64, load: Load, at: f64 },
    /// `load` follows the track `after` (replacing what was said before).
    Next { epoch: u64, after: Key, load: Load },
    /// Nothing follows `after`.
    Last { epoch: u64, after: Key },
    /// Drop what's buffered and play nothing.
    Stop { epoch: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Track `key` is heard from output frame `frame`, `start` seconds in.
    Began { epoch: u64, key: Key, frame: u64, start: f64, duration: Option<f64> },
    /// Say what follows `after` ([`Cmd::Next`] or [`Cmd::Last`]).
    Want { epoch: u64, after: Key },
    /// Track `key` couldn't be played (or not to its end).
    Failed { epoch: u64, key: Key, error: String },
    /// The stream ends at `frame`: nothing follows.
    Finished { epoch: u64, frame: u64 },
}

enum Following {
    Unknown,
    Track(Load),
    Nothing,
}

struct Player {
    rate: u32,
    shared: Arc<Shared>,
    ring: rtrb::Producer<f32>,
    events: UnboundedSender<Event>,
    epoch: u64,
    playing: Option<(Key, Track)>,
    /// The key whose successor was asked for, when nothing is playing until it
    /// comes.
    waiting: Option<Key>,
    following: Following,
    resampler: Option<Resampler>,
    /// Output frames pushed into the ring since the flush, and the output frame
    /// where the resampler's first frame falls.
    pushed: u64,
    base: u64,
    out: Vec<f32>,
}

/// Run the decoder thread until `cmds` closes.
pub fn spawn(
    rate: u32,
    shared: Arc<Shared>,
    ring: rtrb::Producer<f32>,
    cmds: Receiver<Cmd>,
    events: UnboundedSender<Event>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("audiod-decode".into())
        .spawn(move || {
            let mut p = Player {
                rate,
                shared,
                ring,
                events,
                epoch: 0,
                playing: None,
                waiting: None,
                following: Following::Unknown,
                resampler: None,
                pushed: 0,
                base: 0,
                out: Vec::new(),
            };
            p.run(cmds);
        })
        .expect("spawn the decoder thread")
}

impl Player {
    fn run(&mut self, cmds: Receiver<Cmd>) {
        loop {
            // With nothing to decode, wait for something to do.
            let cmd = if self.playing.is_none() {
                match cmds.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                }
            } else {
                match cmds.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            };
            if let Some(cmd) = cmd {
                self.handle(cmd);
                continue;
            }
            self.decode_some();
            if !self.push_out(&cmds) {
                return;
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Start { epoch, load, at } => {
                self.reset(epoch);
                self.begin(load, at);
            }
            Cmd::Stop { epoch } => self.reset(epoch),
            Cmd::Next { epoch, after, load } if epoch == self.epoch => {
                if self.waiting == Some(after) {
                    self.waiting = None;
                    self.begin(load, 0.0);
                } else if self.playing.as_ref().is_some_and(|(k, _)| *k == after) {
                    self.following = Following::Track(load);
                }
            }
            Cmd::Last { epoch, after } if epoch == self.epoch => {
                if self.waiting == Some(after) {
                    self.waiting = None;
                    self.finish();
                } else if self.playing.as_ref().is_some_and(|(k, _)| *k == after) {
                    self.following = Following::Nothing;
                }
            }
            // About a stream that's been dropped since.
            Cmd::Next { .. } | Cmd::Last { .. } => {}
        }
    }

    /// Drop everything buffered, decoded or planned.
    fn reset(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.playing = None;
        self.waiting = None;
        self.following = Following::Unknown;
        self.resampler = None;
        self.out.clear();
        self.pushed = 0;
        self.base = 0;
        self.shared.flush.store(true, Ordering::Release);
        // The mixer drops the ring on its next call; wait for it, but not forever
        // (an output that's stopped calling will drop it whenever it starts).
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.shared.flush.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Start decoding `load` from `at` seconds, straight after what's pushed.
    fn begin(&mut self, load: Load, at: f64) {
        let key = load.key;
        let opened = Track::open(load.file).and_then(|mut t| {
            let start = if at > 0.0 { t.seek(at)? } else { 0.0 };
            Ok((t, start))
        });
        match opened {
            Ok((track, start)) => {
                // A track at another rate needs another resampler: finish the old one.
                if !self.resampler.as_ref().is_some_and(|r| r.converts(track.rate(), self.rate)) {
                    if let Some(mut old) = self.resampler.take() {
                        old.finish(&mut self.out);
                    }
                    self.resampler = Some(Resampler::new(track.rate(), self.rate));
                    self.base = self.written();
                }
                let r = self.resampler.as_ref().expect("made above");
                let frame = self.base + r.output_frame(r.taken());
                let duration = track.duration();
                self.playing = Some((key, track));
                self.following = Following::Unknown;
                self.send(Event::Began { epoch: self.epoch, key, frame, start, duration });
                self.send(Event::Want { epoch: self.epoch, after: key });
            }
            Err(error) => {
                self.send(Event::Failed { epoch: self.epoch, key, error });
                self.waiting = Some(key);
                self.send(Event::Want { epoch: self.epoch, after: key });
            }
        }
    }

    /// Output frames made since the flush: pushed, and waiting to be.
    fn written(&self) -> u64 {
        self.pushed + (self.out.len() / 2) as u64
    }

    /// Decode the next piece of the playing track into `out`, or move on at its end.
    fn decode_some(&mut self) {
        let Some((key, track)) = &mut self.playing else { return };
        let key = *key;
        match track.frames() {
            Ok(Some(frames)) => {
                if let Some(r) = &mut self.resampler {
                    r.process(frames, &mut self.out);
                }
            }
            Ok(None) => self.ended(),
            Err(error) => {
                self.send(Event::Failed { epoch: self.epoch, key, error });
                self.ended();
            }
        }
    }

    /// The playing track is done: go on to what follows.
    fn ended(&mut self) {
        let Some((key, _)) = self.playing.take() else { return };
        match std::mem::replace(&mut self.following, Following::Unknown) {
            Following::Track(load) => self.begin(load, 0.0),
            Following::Nothing => self.finish(),
            Following::Unknown => self.waiting = Some(key),
        }
    }

    /// The stream ends: flush the resampler and say where.
    fn finish(&mut self) {
        if let Some(mut r) = self.resampler.take() {
            r.finish(&mut self.out);
        }
        let frame = self.written();
        self.send(Event::Finished { epoch: self.epoch, frame });
    }

    /// Move `out` into the ring, waiting for room. Commands that arrive meanwhile
    /// are handled; one that drops the stream ends the wait. False when the
    /// service has gone.
    fn push_out(&mut self, cmds: &Receiver<Cmd>) -> bool {
        let mut done = 0;
        while done < self.out.len() {
            let room = self.ring.slots() & !1;
            if room == 0 {
                match cmds.recv_timeout(Duration::from_millis(5)) {
                    Ok(cmd) => {
                        let epoch = self.epoch;
                        self.handle(cmd);
                        if self.epoch != epoch {
                            return true; // `out` belonged to the dropped stream
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
                }
                continue;
            }
            let n = room.min(self.out.len() - done);
            let (pushed, _) = self.ring.push_partial_slice(&self.out[done..done + n]);
            done += pushed.len();
            self.pushed += (pushed.len() / 2) as u64;
        }
        self.out.clear();
        true
    }

    fn send(&self, e: Event) {
        let _ = self.events.send(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::tests::wav;
    use crate::output;
    use std::sync::atomic::AtomicBool;

    /// The decoder thread with a mixer pulled as fast as it fills.
    struct Rig {
        cmds: std::sync::mpsc::Sender<Cmd>,
        events: tokio::sync::mpsc::UnboundedReceiver<Event>,
        shared: Arc<Shared>,
        stop: Arc<AtomicBool>,
    }

    impl Rig {
        fn new() -> Rig {
            let shared = Shared::new(1.0);
            let (producer, mut mixer) = output::ring(48_000, shared.clone());
            let (cmds, rx) = std::sync::mpsc::channel();
            let (tx, events) = tokio::sync::mpsc::unbounded_channel();
            spawn(48_000, shared.clone(), producer, rx, tx);
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0.0; 2 * 4096];
                while !s.load(Ordering::Relaxed) {
                    mixer.fill(&mut buf, 2);
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            Rig { cmds, events, shared, stop }
        }

        fn next(&mut self) -> Event {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(e) = self.events.try_recv() {
                    return e;
                }
                assert!(Instant::now() < deadline, "no event");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn began(key: Key, frame: u64) -> Event {
        Event::Began { epoch: 1, key, frame, start: 0.0, duration: None }
    }

    fn no_duration(e: Event) -> Event {
        match e {
            Event::Began { epoch, key, frame, start, .. } => {
                Event::Began { epoch, key, frame, start, duration: None }
            }
            e => e,
        }
    }

    #[test]
    fn tracks_join_on_the_exact_frame_even_across_rates() {
        let mut rig = Rig::new();
        rig.cmds
            .send(Cmd::Start { epoch: 1, load: Load { key: 1, file: wav(8000, 1, 1.0) }, at: 0.0 })
            .unwrap();
        assert_eq!(no_duration(rig.next()), began(1, 0));
        assert_eq!(rig.next(), Event::Want { epoch: 1, after: 1 });
        // The same rate: one continuous stream through the resampler.
        rig.cmds
            .send(Cmd::Next { epoch: 1, after: 1, load: Load { key: 2, file: wav(8000, 2, 1.0) } })
            .unwrap();
        assert_eq!(no_duration(rig.next()), began(2, 48_000));
        assert_eq!(rig.next(), Event::Want { epoch: 1, after: 2 });
        // Another rate: a new resampler, starting exactly where the last one ended.
        rig.cmds
            .send(Cmd::Next { epoch: 1, after: 2, load: Load { key: 3, file: wav(44_100, 2, 0.5) } })
            .unwrap();
        assert_eq!(no_duration(rig.next()), began(3, 96_000));
        assert_eq!(rig.next(), Event::Want { epoch: 1, after: 3 });
        rig.cmds.send(Cmd::Last { epoch: 1, after: 3 }).unwrap();
        assert_eq!(rig.next(), Event::Finished { epoch: 1, frame: 120_000 });
        // Every frame reaches the mixer: no more, no fewer.
        let deadline = Instant::now() + Duration::from_secs(10);
        while rig.shared.since_flush() < 120_000 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(rig.shared.since_flush(), 120_000);
    }

    #[test]
    fn a_bad_track_is_reported_and_the_next_one_plays() {
        let mut rig = Rig::new();
        let junk = {
            use std::io::{Seek, Write};
            let mut f = wav(8000, 1, 0.1);
            f.set_len(0).unwrap();
            f.write_all(&[1; 999]).unwrap();
            f.rewind().unwrap();
            f
        };
        rig.cmds.send(Cmd::Start { epoch: 1, load: Load { key: 1, file: junk }, at: 0.0 }).unwrap();
        assert!(matches!(rig.next(), Event::Failed { key: 1, .. }));
        assert_eq!(rig.next(), Event::Want { epoch: 1, after: 1 });
        rig.cmds
            .send(Cmd::Next { epoch: 1, after: 1, load: Load { key: 2, file: wav(8000, 1, 0.5) } })
            .unwrap();
        assert_eq!(no_duration(rig.next()), began(2, 0));
    }

    #[test]
    fn starting_again_drops_the_old_stream_and_seeks() {
        let mut rig = Rig::new();
        rig.cmds
            .send(Cmd::Start { epoch: 1, load: Load { key: 1, file: wav(8000, 1, 3.0) }, at: 0.0 })
            .unwrap();
        assert_eq!(no_duration(rig.next()), began(1, 0));
        assert_eq!(rig.next(), Event::Want { epoch: 1, after: 1 });
        rig.cmds
            .send(Cmd::Start { epoch: 2, load: Load { key: 1, file: wav(8000, 1, 3.0) }, at: 2.0 })
            .unwrap();
        assert_eq!(
            no_duration(rig.next()),
            Event::Began { epoch: 2, key: 1, frame: 0, start: 2.0, duration: None }
        );
        assert_eq!(rig.next(), Event::Want { epoch: 2, after: 1 });
        // A late answer about the dropped stream changes nothing.
        rig.cmds.send(Cmd::Last { epoch: 1, after: 1 }).unwrap();
        rig.cmds.send(Cmd::Last { epoch: 2, after: 1 }).unwrap();
        // One second was left after the seek.
        assert_eq!(rig.next(), Event::Finished { epoch: 2, frame: 48_000 });
    }
}
