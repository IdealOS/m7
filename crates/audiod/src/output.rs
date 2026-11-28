//! The sound output: a mixer the output pulls frames from, and where it plays.
//!
//! The decoder thread pushes stereo frames into a lock-free ring; the output's
//! callback (or, with no sound device, a thread keeping time) asks the [`Mixer`]
//! for frames. The mixer never blocks or allocates: it reads the ring, applies
//! the volume, and counts frames played, which is how audiod knows what's
//! audible. The mixer is where other sources (system sounds) would be summed in.
//!
//! Pausing and volume changes ramp over [`RAMP`], so they don't click.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use protocol::audio::OutputDevice;

/// Seconds of audio the ring holds.
pub const RING_SECONDS: f64 = 2.0;
/// How long a change of gain takes.
const RAMP: f64 = 0.01;
/// The rate used when there's no device to ask.
pub const DEFAULT_RATE: u32 = 48_000;

/// What the decoder thread, the mixer and the service share.
#[derive(Default)]
pub struct Shared {
    /// Frames taken from the ring and played, ever.
    pub played: AtomicU64,
    /// Set to drop everything in the ring; the mixer clears it once done.
    pub flush: AtomicBool,
    /// `played` when the last flush was done.
    pub flushed_at: AtomicU64,
    pub paused: AtomicBool,
    /// The volume, 0 to 1, as `f32` bits.
    volume: AtomicU32,
    /// Times the ring ran dry while playing.
    pub underruns: AtomicU64,
}

impl Shared {
    pub fn new(volume: f32) -> Arc<Shared> {
        let s = Shared::default();
        s.volume.store(volume.to_bits(), Ordering::Relaxed);
        Arc::new(s)
    }
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }
    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    /// Frames played since the last flush.
    pub fn since_flush(&self) -> u64 {
        let at = self.flushed_at.load(Ordering::Acquire);
        self.played.load(Ordering::Acquire).saturating_sub(at)
    }
}

pub struct Mixer {
    ring: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
    gain: f32,
    step: f32,
}

/// A ring for `rate` Hz output: the decoder's end, and the mixer.
pub fn ring(rate: u32, shared: Arc<Shared>) -> (rtrb::Producer<f32>, Mixer) {
    let (producer, consumer) = rtrb::RingBuffer::new(2 * (rate as f64 * RING_SECONDS) as usize);
    let step = 1.0 / (rate as f64 * RAMP) as f32;
    (producer, Mixer { ring: consumer, shared, gain: 0.0, step })
}

impl Mixer {
    /// Fill `out`, interleaved in `channels`, with what's next.
    pub fn fill(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        let shared = &*self.shared;
        if shared.flush.load(Ordering::Acquire) {
            let n = self.ring.slots();
            if let Ok(chunk) = self.ring.read_chunk(n) {
                chunk.commit_all();
            }
            shared.flushed_at.store(shared.played.load(Ordering::Relaxed), Ordering::Release);
            shared.flush.store(false, Ordering::Release);
        }
        let paused = shared.paused.load(Ordering::Relaxed);
        let v = shared.volume();
        // Loudness follows the square of the setting more closely than the setting.
        let target = if paused { 0.0 } else { v * v };
        let mut played = 0u64;
        let mut dry = false;
        for frame in out.chunks_exact_mut(channels) {
            // Paused and faded out: hold the ring where it is.
            if paused && self.gain <= 0.0 {
                frame.fill(0.0);
                continue;
            }
            let (l, r) = if self.ring.slots() >= 2 {
                played += 1;
                (self.ring.pop().unwrap_or(0.0), self.ring.pop().unwrap_or(0.0))
            } else {
                dry = true;
                (0.0, 0.0)
            };
            if self.gain < target {
                self.gain = (self.gain + self.step).min(target);
            } else if self.gain > target {
                self.gain = (self.gain - self.step).max(target);
            }
            match channels {
                1 => frame[0] = (l + r) * 0.5 * self.gain,
                _ => {
                    frame[0] = l * self.gain;
                    frame[1] = r * self.gain;
                    frame[2..].fill(0.0);
                }
            }
        }
        if dry && !paused {
            shared.underruns.fetch_add(1, Ordering::Relaxed);
        }
        shared.played.fetch_add(played, Ordering::Release);
    }
}

/// Where sound goes.
pub struct Output {
    /// The device's name, or none when playing to nowhere.
    pub name: Option<String>,
    /// Its id, as [`outputs`] gives it.
    pub id: Option<String>,
    pub rate: u32,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

impl Output {
    /// Play to nowhere, keeping time: `speed` times real time (1 for real use;
    /// tests go faster).
    pub fn null(rate: u32, speed: f64, make: impl FnOnce(u32) -> Mixer) -> Output {
        let mut mixer = make(rate);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("audiod-null".into())
                .spawn(move || {
                    let start = Instant::now();
                    let mut done = 0u64;
                    let mut buf = vec![0.0f32; 2 * 4096];
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(5));
                        let due = (start.elapsed().as_secs_f64() * rate as f64 * speed) as u64;
                        while done < due {
                            let n = ((due - done) as usize).min(4096);
                            mixer.fill(&mut buf[..2 * n], 2);
                            done += n as u64;
                        }
                    }
                })
                .expect("spawn the null output")
        };
        Output { name: None, id: None, rate, stop, thread: Some(thread) }
    }

    /// Play to the sound device `device`: the one with that id (as [`outputs`]
    /// gives it), or failing that any whose name contains it; or the default
    /// one. Errors if there's none.
    pub fn device(
        device: Option<&str>,
        make: impl FnOnce(u32) -> Mixer + Send + 'static,
    ) -> Result<Output, String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let want = device.map(str::to_owned);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<Result<(String, Option<String>, u32), String>>();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("audiod-output".into())
                .spawn(move || {
                    let host = cpal::default_host();
                    let found = match &want {
                        None => host.default_output_device(),
                        Some(w) => {
                            let all: Vec<cpal::Device> =
                                host.output_devices().map(Iterator::collect).unwrap_or_default();
                            let by_id = all.iter().position(|d| d.id().is_ok_and(|i| i.id() == w.as_str()));
                            let by_name = || {
                                all.iter().position(|d| {
                                    d.description().is_ok_and(|n| n.name().contains(w.as_str()))
                                })
                            };
                            by_id.or_else(by_name).map(|i| all[i].clone())
                        }
                    };
                    let Some(dev) = found else {
                        let _ = tx.send(Err(match &want {
                            Some(w) => format!("no sound output named {w:?}"),
                            None => "no sound output".into(),
                        }));
                        return;
                    };
                    let described = describe(&dev);
                    let id = described.as_ref().map(|o| o.id.clone());
                    let name = described.map_or_else(|| "sound output".into(), |o| o.name);
                    let config = match dev.default_output_config() {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(Err(format!("{name}: {e}")));
                            return;
                        }
                    };
                    let format = config.sample_format();
                    let config: cpal::StreamConfig = config.into();
                    let (rate, channels) = (config.sample_rate, config.channels as usize);
                    let mixer = make(rate);
                    // The mixer counts underruns itself; CoreAudio reports one as a
                    // stream starts.
                    let err = |e: cpal::Error| match e.kind() {
                        cpal::ErrorKind::Xrun => log::debug!("audiod: sound output: {e}"),
                        _ => log::warn!("audiod: sound output: {e}"),
                    };
                    let stream = match format {
                        cpal::SampleFormat::F32 => dev.build_output_stream(
                            config,
                            {
                                let mut mixer = mixer;
                                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                    mixer.fill(data, channels)
                                }
                            },
                            err,
                            None,
                        ),
                        cpal::SampleFormat::I16 => dev.build_output_stream(
                            config,
                            converting(mixer, channels, |v: f32| {
                                (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
                            }),
                            err,
                            None,
                        ),
                        cpal::SampleFormat::I32 => dev.build_output_stream(
                            config,
                            converting(mixer, channels, |v: f32| {
                                (v.clamp(-1.0, 1.0) as f64 * i32::MAX as f64) as i32
                            }),
                            err,
                            None,
                        ),
                        other => {
                            let _ = tx.send(Err(format!("{name}: can't play {other} samples")));
                            return;
                        }
                    };
                    let stream = match stream.and_then(|s| s.play().map(|()| s)) {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = tx.send(Err(format!("{name}: {e}")));
                            return;
                        }
                    };
                    let _ = tx.send(Ok((name, id, rate)));
                    // The stream plays while this thread holds it.
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::park();
                    }
                    drop(stream);
                })
                .map_err(|e| e.to_string())?
        };
        match rx.recv() {
            Ok(Ok((name, id, rate))) => Ok(Output { name: Some(name), id, rate, stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err("the sound output thread stopped".into())
            }
        }
    }
}

/// The sound outputs there are now, to choose from, by name. ALSA lists each
/// card many ways (`hw:`, `plughw:`, `dmix:`, `front:`...); of those, one per
/// card is offered: `hdmi:` for an HDMI port, else `sysdefault:`, else
/// `plughw:`, which convert what they're given. Elsewhere, every output device.
pub fn outputs() -> Vec<OutputDevice> {
    use cpal::traits::HostTrait;
    let host = cpal::default_host();
    let all: Vec<OutputDevice> =
        host.output_devices().map(|ds| ds.filter_map(|d| describe(&d)).collect()).unwrap_or_default();
    let mut picked = per_card(&all);
    if picked.is_empty() {
        picked = all;
    }
    picked.sort_by(|a, b| a.name.cmp(&b.name));
    picked.dedup_by(|a, b| a.id == b.id);
    picked
}

/// A device's id and the name to show for it.
fn describe(dev: &cpal::Device) -> Option<OutputDevice> {
    use cpal::traits::DeviceTrait;
    let id = dev.id().ok()?.id().to_owned();
    let desc = dev.description().ok()?;
    Some(OutputDevice { name: friendly(&id, desc.name()), id })
}

/// Of ALSA's many names for each card, the one to offer: see [`outputs`].
fn per_card(all: &[OutputDevice]) -> Vec<OutputDevice> {
    let mut best: Vec<(String, usize, &OutputDevice)> = Vec::new();
    for o in all {
        let Some((kind, card)) = alsa_card(&o.id) else { continue };
        let rank = match kind {
            "hdmi" => 0,
            "sysdefault" => 1,
            "plughw" => 2,
            _ => continue,
        };
        match best.iter_mut().find(|(c, ..)| *c == card) {
            Some(b) if rank < b.1 => *b = (card, rank, o),
            Some(_) => {}
            None => best.push((card, rank, o)),
        }
    }
    // cpal also lists each card by number (`plughw:CARD=0,DEV=0`): drop those
    // that are a card already offered by name.
    // Compared by ALSA's own name for the card ("Card, Device"'s first part).
    let shown = |o: &OutputDevice| o.name.split(", ").next().unwrap_or(&o.name).trim().to_owned();
    let named: Vec<String> = best.iter().filter(|(c, ..)| !is_number(c)).map(|(.., o)| shown(o)).collect();
    best.into_iter()
        .filter(|(c, _, o)| !is_number(c) || !named.contains(&shown(o)))
        .map(|(.., o)| o.clone())
        .collect()
}

fn is_number(card: &str) -> bool {
    !card.is_empty() && card.bytes().all(|b| b.is_ascii_digit())
}

/// An ALSA device id's kind and card: `("hdmi", "vc4hdmi0")` for
/// `hdmi:CARD=vc4hdmi0,DEV=0`.
fn alsa_card(id: &str) -> Option<(&str, String)> {
    let (kind, rest) = id.split_once(':')?;
    let card = rest.split(',').find_map(|p| p.strip_prefix("CARD="))?;
    Some((kind, card.to_owned()))
}

/// The name to show for output `id`, which its system calls `name`. ALSA
/// describes a card as "Card, Device" (often the same twice): the card, with the
/// Pi's own outputs named for what they are.
fn friendly(id: &str, name: &str) -> String {
    if let Some((_, card)) = alsa_card(id) {
        if let Some(n) = card.strip_prefix("vc4hdmi").and_then(|n| n.parse::<u32>().ok()) {
            return format!("HDMI {}", n + 1);
        }
        if card == "Headphones" {
            return "Headphones".into();
        }
        let first = name.split(", ").next().unwrap_or(name).trim();
        if !first.is_empty() {
            return first.to_owned();
        }
        return card;
    }
    name.to_owned()
}

/// A callback for devices that take integer samples: mix as `f32`, then convert.
fn converting<T: Copy + Default>(
    mut mixer: Mixer,
    channels: usize,
    convert: impl Fn(f32) -> T + Send + 'static,
) -> impl FnMut(&mut [T], &cpal::OutputCallbackInfo) + Send + 'static {
    let mut scratch: Vec<f32> = Vec::with_capacity(16_384);
    move |data: &mut [T], _| {
        scratch.resize(data.len(), 0.0);
        mixer.fill(&mut scratch, channels);
        for (d, s) in data.iter_mut().zip(&scratch) {
            *d = convert(*s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str) -> OutputDevice {
        OutputDevice { id: id.into(), name: name.into() }
    }

    #[test]
    fn alsa_offers_one_output_per_card_named_plainly() {
        // As `aplay -L` lists a Pi 4 with a USB DAC.
        let all = [
            dev("default", "Playback/recording through the PulseAudio sound server"),
            dev("hw:CARD=Headphones,DEV=0", "bcm2835 Headphones, bcm2835 Headphones"),
            dev("plughw:CARD=Headphones,DEV=0", "bcm2835 Headphones, bcm2835 Headphones"),
            dev("sysdefault:CARD=Headphones", "bcm2835 Headphones, bcm2835 Headphones"),
            dev("dmix:CARD=Headphones,DEV=0", "bcm2835 Headphones, bcm2835 Headphones"),
            dev("hdmi:CARD=vc4hdmi0,DEV=0", "vc4-hdmi-0, MAI PCM i2s-hifi-0"),
            dev("sysdefault:CARD=vc4hdmi0", "vc4-hdmi-0, MAI PCM i2s-hifi-0"),
            dev("hdmi:CARD=vc4hdmi1,DEV=0", "vc4-hdmi-1, MAI PCM i2s-hifi-0"),
            dev("plughw:CARD=DAC,DEV=0", "USB Audio DAC, USB Audio"),
            dev("front:CARD=DAC,DEV=0", "USB Audio DAC, USB Audio"),
            // cpal's own entries for the cards, by number: the same cards again.
            dev("plughw:CARD=0,DEV=0", "bcm2835 Headphones, bcm2835 Headphones"),
            dev("plughw:CARD=3,DEV=0", "USB Audio DAC, USB Audio"),
            // A card only listed by number stays.
            dev("plughw:CARD=4,DEV=0", "Other Card, Other Card"),
        ];
        let named: Vec<(String, String)> =
            per_card(&all).into_iter().map(|o| (friendly(&o.id, &o.name), o.id)).collect();
        assert_eq!(
            named,
            [
                ("Headphones".to_owned(), "sysdefault:CARD=Headphones".to_owned()),
                ("HDMI 1".to_owned(), "hdmi:CARD=vc4hdmi0,DEV=0".to_owned()),
                ("HDMI 2".to_owned(), "hdmi:CARD=vc4hdmi1,DEV=0".to_owned()),
                ("USB Audio DAC".to_owned(), "plughw:CARD=DAC,DEV=0".to_owned()),
                ("Other Card".to_owned(), "plughw:CARD=4,DEV=0".to_owned()),
            ]
        );
        assert_eq!(friendly("BuiltInSpeakerDevice", "MacBook Air Speakers"), "MacBook Air Speakers");
    }

    fn filled(n: usize) -> (rtrb::Producer<f32>, Mixer, Arc<Shared>) {
        let shared = Shared::new(1.0);
        let (mut p, m) = ring(1000, shared.clone());
        for i in 0..n {
            p.push(i as f32).unwrap();
            p.push(-(i as f32)).unwrap();
        }
        (p, m, shared)
    }

    #[test]
    fn plays_frames_in_order_and_counts_them() {
        let (_p, mut m, shared) = filled(100);
        let mut out = vec![0.0; 2 * 50];
        m.fill(&mut out, 2);
        assert_eq!(shared.played.load(Ordering::Relaxed), 50);
        // The gain ramps up over the first 10 frames (10 ms at 1 kHz), then it's unity.
        assert_eq!(out[2 * 20], 20.0);
        assert_eq!(out[2 * 20 + 1], -20.0);
        assert!(out[2] < 1.0);
    }

    #[test]
    fn a_dry_ring_plays_silence_and_counts_only_real_frames() {
        let (_p, mut m, shared) = filled(5);
        let mut out = vec![1.0; 2 * 20];
        m.fill(&mut out, 2);
        assert_eq!(shared.played.load(Ordering::Relaxed), 5);
        assert!(out[10..].iter().all(|&v| v == 0.0));
        assert_eq!(shared.underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn pausing_fades_out_then_holds_the_ring() {
        let (_p, mut m, shared) = filled(500);
        let mut out = vec![0.0; 2 * 100];
        m.fill(&mut out, 2);
        shared.paused.store(true, Ordering::Relaxed);
        m.fill(&mut out, 2);
        // It played on while fading (10 frames), then stopped taking frames.
        assert_eq!(shared.played.load(Ordering::Relaxed), 110);
        assert!(out[2 * 20..].iter().all(|&v| v == 0.0));
        assert_eq!(shared.underruns.load(Ordering::Relaxed), 0, "pausing isn't running dry");
        shared.paused.store(false, Ordering::Relaxed);
        m.fill(&mut out, 2);
        assert_eq!(shared.played.load(Ordering::Relaxed), 210);
    }

    #[test]
    fn a_flush_drops_whats_queued() {
        let (_p, mut m, shared) = filled(500);
        let mut out = vec![0.0; 2 * 100];
        m.fill(&mut out, 2);
        shared.flush.store(true, Ordering::Release);
        m.fill(&mut out, 2);
        assert!(!shared.flush.load(Ordering::Acquire));
        assert_eq!(shared.flushed_at.load(Ordering::Acquire), 100);
        assert_eq!(shared.since_flush(), 0);
        assert!(out.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn volume_scales_and_extra_channels_are_silent() {
        let (_p, mut m, shared) = filled(100);
        shared.set_volume(0.5);
        let mut out = vec![9.0; 4 * 50];
        m.fill(&mut out, 4);
        // 0.5 on the dial is a quarter of the amplitude.
        assert_eq!(out[4 * 40], 40.0 * 0.25);
        assert_eq!(out[4 * 40 + 2], 0.0);
        assert_eq!(out[4 * 40 + 3], 0.0);
        shared.set_volume(7.0);
        assert_eq!(shared.volume(), 1.0);
    }
}
