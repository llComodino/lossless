use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use lossless_proto::StreamFormat;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{TimeBase, Timestamp};

fn ratio(tb: TimeBase) -> (u32, u32) {
    (tb.numer.get(), tb.denom.get())
}

/// Frames at `rate` for timestamp `ts` in time base `numer/denom` seconds
/// (no time base: the timestamp is already in frames). Saturates rather than
/// wrapping; can't overflow in u128 (i64 * u32 * u32 < 2^128).
fn ts_to_frames(ts: i64, time_base: Option<(u32, u32)>, rate: u32) -> u64 {
    let ts = ts.max(0) as u128;
    let frames = match time_base {
        Some((numer, denom)) => {
            ts * u128::from(numer) * u128::from(rate) / u128::from(denom.max(1))
        }
        None => ts,
    };
    u64::try_from(frames).unwrap_or(u64::MAX)
}

/// Inverse of `ts_to_frames`, rounding up so the timestamp is never before
/// `frame`. Exact for FLAC/MP3, whose time base is 1/rate.
fn frames_to_ts(frame: u64, time_base: Option<(u32, u32)>, rate: u32) -> Option<i64> {
    let ts = match time_base {
        Some((numer, denom)) => {
            let per = u128::from(numer) * u128::from(rate);
            if per == 0 {
                return None;
            }
            (u128::from(frame) * u128::from(denom)).div_ceil(per)
        }
        None => u128::from(frame),
    };
    i64::try_from(ts).ok()
}

/// One decoded file: interleaved full-scale i32 chunks, seekable to the frame.
pub struct Source {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    pub format: StreamFormat,
    pub total_frames: Option<u64>,
    /// Frames still to drop after a seek landed before the target.
    skip: u64,
    buf: Vec<i32>,
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let ext = path.extension().and_then(|e| e.to_str());
        Self::from_media(Box::new(file), ext).with_context(|| path.display().to_string())
    }

    /// Decode a file held in memory; `ext` is a format hint ("flac", "mp3").
    pub fn from_bytes(bytes: Vec<u8>, ext: Option<&str>) -> Result<Self> {
        Self::from_media(Box::new(std::io::Cursor::new(bytes)), ext)
    }

    fn from_media(
        media: Box<dyn symphonia::core::io::MediaSource>,
        ext: Option<&str>,
    ) -> Result<Self> {
        let mss = MediaSourceStream::new(media, Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = ext {
            hint.with_extension(ext);
        }
        let reader = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .context("probing format")?;
        let track = reader
            .default_track(TrackType::Audio)
            .ok_or_else(|| anyhow!("no audio track"))?;
        let track_id = track.id;
        let time_base = track.time_base;
        let total_frames = track.num_frames;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| anyhow!("no audio codec parameters"))?
            .clone();
        let rate = params
            .sample_rate
            .filter(|&r| r > 0)
            .ok_or_else(|| anyhow!("unknown or zero sample rate"))?;
        let channels = params
            .channels
            .as_ref()
            .map(|c| c.count() as u16)
            .ok_or_else(|| anyhow!("unknown channel count"))?;
        // Lossy codecs (MP3) have no native depth: send them at 32-bit.
        let bits = params.bits_per_sample.unwrap_or(32) as u16;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .context("creating decoder")?;
        Ok(Self {
            reader,
            decoder,
            track_id,
            time_base,
            format: StreamFormat {
                rate,
                bits,
                channels,
            },
            total_frames,
            skip: 0,
            buf: Vec::new(),
        })
    }

    fn ts_to_frames(&self, ts: Timestamp) -> u64 {
        ts_to_frames(ts.get(), self.time_base.map(ratio), self.format.rate)
    }

    /// Next chunk of interleaved samples, or `None` at the end of the file.
    pub fn next_chunk(&mut self) -> Result<Option<&[i32]>> {
        let channels = self.format.channels as usize;
        loop {
            let Some(packet) = self.reader.next_packet().context("reading packet")? else {
                return Ok(None);
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let buf = match self.decoder.decode(&packet) {
                Ok(buf) => buf,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(e) => return Err(e).context("decoding"),
            };
            self.buf.resize(buf.samples_interleaved(), 0);
            buf.copy_to_slice_interleaved(&mut self.buf);
            let frames = (self.buf.len() / channels) as u64;
            if self.skip >= frames {
                self.skip -= frames;
                continue;
            }
            let start = self.skip as usize * channels;
            self.skip = 0;
            return Ok(Some(&self.buf[start..]));
        }
    }

    fn frames_to_ts(&self, frame: u64) -> Option<Timestamp> {
        frames_to_ts(frame, self.time_base.map(ratio), self.format.rate).map(Timestamp::from)
    }

    /// Seek so the next chunk starts exactly at `frame`. Returns that frame.
    /// Seeks by timestamp in the track's time base: integer-exact, unlike
    /// going through seconds.
    pub fn seek(&mut self, frame: u64) -> Result<u64> {
        let ts = self
            .frames_to_ts(frame)
            .ok_or_else(|| anyhow!("seek target out of range"))?;
        let seeked = self
            .reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts,
                    track_id: self.track_id,
                },
            )
            .context("seeking")?;
        self.decoder.reset();
        let required = self.ts_to_frames(seeked.required_ts);
        let actual = self.ts_to_frames(seeked.actual_ts);
        self.skip = required.saturating_sub(actual);
        Ok(required)
    }
}

#[cfg(test)]
mod tests {
    use super::Source;
    use crate::fixtures::{BLOCK, Dir, Fixture, flac, write_bytes};

    fn decode_all(src: &mut Source) -> Vec<i32> {
        let mut out = Vec::new();
        while let Some(chunk) = src.next_chunk().unwrap() {
            out.extend_from_slice(chunk);
        }
        out
    }

    fn check_exact(f: &Fixture) {
        let mut src = Source::open(&f.path).unwrap();
        assert_eq!(src.format.rate, f.rate);
        assert_eq!(src.format.bits, f.bits);
        assert_eq!(src.format.channels, f.channels);
        assert_eq!(src.total_frames, Some(f.frames()));
        assert!(decode_all(&mut src) == f.expected, "decoded samples differ");
    }

    #[test]
    fn decodes_16_bit_exactly() {
        let d = Dir::new();
        check_exact(&flac(&d, "a.flac", 44_100, 16, 2, 3 * BLOCK));
    }

    #[test]
    fn decodes_24_bit_exactly() {
        let d = Dir::new();
        // flacenc caps fixtures at 96 kHz; 24/192 is verified on hardware.
        check_exact(&flac(&d, "a.flac", 48_000, 24, 2, 3 * BLOCK));
        check_exact(&flac(&d, "b.flac", 96_000, 24, 2, 3 * BLOCK));
    }

    #[test]
    fn decodes_mono_exactly() {
        let d = Dir::new();
        check_exact(&flac(&d, "a.flac", 48_000, 16, 1, 2 * BLOCK));
    }

    #[test]
    fn seek_is_sample_accurate() {
        let d = Dir::new();
        let f = flac(&d, "a.flac", 44_100, 16, 2, 5 * BLOCK);
        let ch = f.channels as usize;
        let mut src = Source::open(&f.path).unwrap();
        // Off block boundaries, at a boundary, at the start, at the last frame.
        for frame in [12_345, 4_096, 0, 5 * BLOCK as u64 - 1, 7] {
            assert_eq!(src.seek(frame).unwrap(), frame);
            let rest = decode_all(&mut src);
            assert!(
                rest == f.expected[frame as usize * ch..],
                "samples after seek to {frame} differ"
            );
        }
    }

    #[test]
    fn seek_is_sample_accurate_at_24_bit() {
        let d = Dir::new();
        let f = flac(&d, "a.flac", 96_000, 24, 2, 3 * BLOCK);
        let mut src = Source::open(&f.path).unwrap();
        assert_eq!(src.seek(9_001).unwrap(), 9_001);
        assert!(decode_all(&mut src) == f.expected[9_001 * 2..]);
    }

    #[test]
    fn decodes_from_memory_exactly() {
        let d = Dir::new();
        let f = flac(&d, "a.flac", 44_100, 16, 2, 2 * BLOCK);
        let bytes = std::fs::read(&f.path).unwrap();
        for ext in [Some("flac"), None] {
            let mut src = Source::from_bytes(bytes.clone(), ext).unwrap();
            assert!(decode_all(&mut src) == f.expected);
        }
        assert!(Source::from_bytes(Vec::new(), Some("flac")).is_err());
    }

    #[test]
    fn seek_past_end_is_an_error_not_a_panic() {
        let d = Dir::new();
        let f = flac(&d, "a.flac", 44_100, 16, 2, BLOCK);
        let mut src = Source::open(&f.path).unwrap();
        assert!(src.seek(1_000_000).is_err());
    }

    #[test]
    fn bad_files_are_errors() {
        let d = Dir::new();
        assert!(Source::open(&d.path("missing.flac")).is_err());
        assert!(Source::open(&write_bytes(&d, "empty.flac", b"")).is_err());
        assert!(Source::open(&write_bytes(&d, "junk.flac", &[0x5a; 4096])).is_err());
        let f = flac(&d, "a.flac", 44_100, 16, 2, 3 * BLOCK);
        let bytes = std::fs::read(&f.path).unwrap();
        let cut = write_bytes(&d, "cut.flac", &bytes[..bytes.len() / 2]);
        // A truncated file opens and decodes what is there without panicking.
        if let Ok(mut src) = Source::open(&cut) {
            while let Ok(Some(_)) = src.next_chunk() {}
        }
    }
}

#[cfg(test)]
mod props {
    use proptest::prelude::*;

    use super::{frames_to_ts, ts_to_frames};

    /// Random values with the boundaries mixed in: plain random sampling
    /// almost never produces 0 or MAX.
    fn edgy_u32() -> impl Strategy<Value = u32> {
        prop_oneof![
            Just(0),
            Just(1),
            Just(u32::MAX),
            Just(u32::MAX - 1),
            any::<u32>()
        ]
    }

    fn edgy_i64() -> impl Strategy<Value = i64> {
        prop_oneof![
            Just(0),
            Just(-1),
            Just(i64::MAX),
            Just(i64::MIN),
            any::<i64>()
        ]
    }

    proptest! {
        /// More timestamp never means fewer frames (a wrapping cast would).
        #[test]
        fn ts_to_frames_is_monotonic(
            a in edgy_i64(),
            b in edgy_i64(),
            numer in edgy_u32().prop_filter("nonzero", |n| *n > 0),
            denom in edgy_u32().prop_filter("nonzero", |n| *n > 0),
            rate in edgy_u32(),
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            let tb = Some((numer, denom));
            prop_assert!(ts_to_frames(lo, tb, rate) <= ts_to_frames(hi, tb, rate));
        }

        /// FLAC/MP3 time base (1/rate): timestamps are frames, both ways.
        #[test]
        fn native_time_base_is_identity(frame in 0u64..=i64::MAX as u64, rate in 1u32..=768_000) {
            let tb = Some((1, rate));
            prop_assert_eq!(frames_to_ts(frame, tb, rate), Some(frame as i64));
            prop_assert_eq!(ts_to_frames(frame as i64, tb, rate), frame);
        }

        /// A seek target converted to a timestamp and back never lands
        /// before the requested frame, for any time base and rate.
        #[test]
        fn seek_timestamp_never_lands_early(
            frame in any::<u64>(),
            numer in edgy_u32().prop_filter("nonzero", |n| *n > 0),
            denom in edgy_u32().prop_filter("nonzero", |n| *n > 0),
            rate in edgy_u32(),
        ) {
            if let Some(ts) = frames_to_ts(frame, Some((numer, denom)), rate) {
                prop_assert!(ts_to_frames(ts, Some((numer, denom)), rate) >= frame);
            }
        }

        #[test]
        fn conversions_never_panic(
            ts in edgy_i64(),
            frame in prop_oneof![Just(0), Just(u64::MAX), any::<u64>()],
            tb in proptest::option::of((1u32.., 1u32..)),
            rate in edgy_u32(),
        ) {
            let _ = ts_to_frames(ts, tb, rate);
            let _ = frames_to_ts(frame, tb, rate);
        }
    }
}
