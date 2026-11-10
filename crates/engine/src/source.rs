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
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let reader = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .with_context(|| format!("probing {}", path.display()))?;
        let track = reader
            .default_track(TrackType::Audio)
            .ok_or_else(|| anyhow!("no audio track in {}", path.display()))?;
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
            .ok_or_else(|| anyhow!("unknown sample rate"))?;
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
        let ts = ts.get().max(0) as u128;
        match self.time_base {
            Some(tb) => {
                (ts * u128::from(tb.numer.get()) * u128::from(self.format.rate)
                    / u128::from(tb.denom.get())) as u64
            }
            None => ts as u64,
        }
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

    /// Inverse of `ts_to_frames`, rounding up so the timestamp is never
    /// before `frame`. Exact for FLAC/MP3, whose time base is 1/rate.
    fn frames_to_ts(&self, frame: u64) -> Option<Timestamp> {
        let ts = match self.time_base {
            Some(tb) => {
                let num = u128::from(frame) * u128::from(tb.denom.get());
                let den = u128::from(tb.numer.get()) * u128::from(self.format.rate);
                num.div_ceil(den)
            }
            None => u128::from(frame),
        };
        i64::try_from(ts).ok().map(Timestamp::from)
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
