//! Decoding a track's audio, with symphonia, to stereo `f32` frames.
//!
//! audiod is handed the open blob, so nothing here opens files. Mono is played on
//! both sides, and of more than two channels the front left and right are kept.

use std::fs::File;

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase, Timestamp};

/// Decode errors in a row after which a track is given up on.
const MAX_BAD_PACKETS: u32 = 16;

pub struct Track {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    rate: u32,
    duration: Option<f64>,
    /// Frames still to drop after a seek landed early.
    skip: u64,
    /// The decoded packet, interleaved in its own channels, then as stereo.
    raw: Vec<f32>,
    stereo: Vec<f32>,
}

impl Track {
    /// Read `file`'s format and get ready to decode it.
    pub fn open(file: File) -> Result<Track, String> {
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let reader = symphonia::default::get_probe()
            .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| format!("not audio symphonia can read: {e}"))?;
        let track = reader.default_track(TrackType::Audio).ok_or("it has no audio")?;
        let params =
            track.codec_params.as_ref().and_then(|p| p.audio()).ok_or("its audio has no codec parameters")?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| format!("no decoder for it: {e}"))?;
        let rate = params.sample_rate.filter(|&r| r > 0).ok_or("its audio has no sample rate")?;
        let duration = track.num_frames.map(|n| n as f64 / rate as f64);
        Ok(Track {
            track_id: track.id,
            time_base: track.time_base,
            rate,
            duration,
            reader,
            decoder,
            skip: 0,
            raw: Vec::new(),
            stereo: Vec::new(),
        })
    }

    /// Its sample rate.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Its length in seconds, if the format says.
    pub fn duration(&self) -> Option<f64> {
        self.duration
    }

    /// Go to `secs` from the start; where it is now.
    pub fn seek(&mut self, secs: f64) -> Result<f64, String> {
        let time = Time::try_from_secs_f64(secs.max(0.0)).ok_or("can't seek there")?;
        let to = SeekTo::Time { time, track_id: Some(self.track_id) };
        let seeked = self.reader.seek(SeekMode::Accurate, to).map_err(|e| format!("seeking: {e}"))?;
        self.decoder.reset();
        // An accurate seek lands at or before the time asked for: decode from there
        // and drop what comes before.
        self.skip = match self.time_base {
            Some(tb) => {
                let early = seeked.required_ts.get().saturating_sub(seeked.actual_ts.get()).max(0);
                let secs = tb.calc_time(Timestamp::new(early)).map(|t| t.as_secs_f64()).unwrap_or(0.0);
                (secs * self.rate as f64).round() as u64
            }
            None => 0,
        };
        Ok(secs.max(0.0))
    }

    /// The next stereo frames, interleaved; `None` at the end.
    pub fn frames(&mut self) -> Result<Option<&[f32]>, String> {
        let mut bad = 0;
        loop {
            let packet = match self.reader.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(None),
                // A file cut short ends where it ends.
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(format!("reading it: {e}")),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let buf = match self.decoder.decode(&packet) {
                Ok(buf) => buf,
                // One damaged packet is skipped; many mean the file is no good.
                Err(Error::DecodeError(e)) => {
                    bad += 1;
                    if bad >= MAX_BAD_PACKETS {
                        return Err(format!("decoding it: {e}"));
                    }
                    continue;
                }
                Err(e) => return Err(format!("decoding it: {e}")),
            };
            let channels = buf.spec().channels().count().max(1);
            self.raw.resize(buf.samples_interleaved(), 0.0);
            buf.copy_to_slice_interleaved(&mut self.raw);
            self.stereo.clear();
            for frame in self.raw.chunks_exact(channels) {
                let (l, r) = if channels == 1 { (frame[0], frame[0]) } else { (frame[0], frame[1]) };
                self.stereo.push(l);
                self.stereo.push(r);
            }
            let frames = (self.stereo.len() / 2) as u64;
            if self.skip >= frames {
                self.skip -= frames;
                continue;
            }
            let from = 2 * self.skip as usize;
            self.skip = 0;
            if from == self.stereo.len() {
                continue;
            }
            return Ok(Some(&self.stereo[from..]));
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Write;

    /// A WAV file of `secs` seconds of a tone at `rate` Hz, with `channels`.
    pub fn wav(rate: u32, channels: u16, secs: f64) -> File {
        let frames = (rate as f64 * secs) as u32;
        let mut data = Vec::new();
        for n in 0..frames {
            let v = ((2.0 * std::f64::consts::PI * 440.0 * n as f64 / rate as f64).sin() * 8000.0) as i16;
            for c in 0..channels {
                data.extend((if c == 0 { v } else { -v }).to_le_bytes());
            }
        }
        let mut f = Vec::new();
        f.extend(b"RIFF");
        f.extend((36 + data.len() as u32).to_le_bytes());
        f.extend(b"WAVEfmt ");
        f.extend(16u32.to_le_bytes());
        f.extend(1u16.to_le_bytes());
        f.extend(channels.to_le_bytes());
        f.extend(rate.to_le_bytes());
        f.extend((rate * channels as u32 * 2).to_le_bytes());
        f.extend((channels * 2).to_le_bytes());
        f.extend(16u16.to_le_bytes());
        f.extend(b"data");
        f.extend((data.len() as u32).to_le_bytes());
        f.extend(data);
        let mut file = tempfile();
        file.write_all(&f).unwrap();
        use std::io::Seek;
        file.rewind().unwrap();
        file
    }

    /// An unnamed file that's gone when closed.
    fn tempfile() -> File {
        let path = std::env::temp_dir().join(format!(
            "audiod-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        file
    }

    fn all_frames(t: &mut Track) -> Vec<f32> {
        let mut out = Vec::new();
        while let Some(chunk) = t.frames().unwrap() {
            out.extend_from_slice(chunk);
        }
        out
    }

    #[test]
    fn decodes_every_frame_as_stereo() {
        let mut t = Track::open(wav(8000, 1, 1.0)).unwrap();
        assert_eq!(t.rate(), 8000);
        assert_eq!(t.duration(), Some(1.0));
        let frames = all_frames(&mut t);
        assert_eq!(frames.len(), 2 * 8000);
        // Mono plays on both sides.
        assert!(frames.chunks(2).all(|f| f[0] == f[1]));
        let mut t = Track::open(wav(44_100, 2, 0.5)).unwrap();
        let frames = all_frames(&mut t);
        assert_eq!(frames.len(), 2 * 22_050);
        assert!(frames.chunks(2).skip(10).take(100).all(|f| f[0] == -f[1]));
    }

    #[test]
    fn seeking_lands_on_the_frame() {
        let mut t = Track::open(wav(8000, 1, 2.0)).unwrap();
        assert_eq!(t.seek(1.5).unwrap(), 1.5);
        assert_eq!(all_frames(&mut t).len(), 2 * 4000);
        assert_eq!(t.seek(0.0).unwrap(), 0.0);
        assert_eq!(all_frames(&mut t).len(), 2 * 16_000);
    }

    #[test]
    fn junk_is_refused_when_opened() {
        let mut f = tempfile();
        f.write_all(&[7u8; 4096]).unwrap();
        use std::io::Seek;
        f.rewind().unwrap();
        assert!(Track::open(f).is_err());
    }
}
