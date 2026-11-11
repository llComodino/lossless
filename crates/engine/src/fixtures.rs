//! Test-only FLAC fixtures with known samples (feature `fixtures`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use flacenc::component::BitRepr;
use flacenc::error::Verify;
use lossless_proto::StreamFormat;

use crate::{Result, Sink};

/// A generated FLAC file and the samples a bit-perfect decoder must return
/// for it: interleaved, shifted to full-scale i32 like symphonia does.
pub struct Fixture {
    pub path: PathBuf,
    pub rate: u32,
    pub bits: u16,
    pub channels: u16,
    pub expected: Vec<i32>,
}

impl Fixture {
    pub fn frames(&self) -> u64 {
        (self.expected.len() / self.channels as usize) as u64
    }
}

/// A directory removed when dropped (kept if `LOSSLESS_KEEP_FIXTURES` is set,
/// for inspecting files with `flac -t` / `metaflac`).
pub struct Dir(pub PathBuf);

impl Dir {
    pub fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "lossless-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Default for Dir {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        if std::env::var_os("LOSSLESS_KEEP_FIXTURES").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Deterministic full-range noise: every bit, including the lowest, varies,
/// so an off-by-one in any shift shows up.
fn noise(seed: u32, bits: u16, n: usize) -> Vec<i32> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    let shift = 32 - u32::from(bits);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as i32) >> shift
        })
        .collect()
}

/// flacenc's block size. Fixtures are whole blocks: flacenc records a short
/// final block as the STREAMINFO minimum, which marks the stream variable-
/// block-size while numbering frames as fixed, and symphonia rejects that.
/// Real encoders don't do this.
pub const BLOCK: usize = 4096;

/// Encode `frames` frames of noise as FLAC at `dir/name`. flacenc only
/// accepts rates up to 96 kHz.
pub fn flac(dir: &Dir, name: &str, rate: u32, bits: u16, channels: u16, frames: usize) -> Fixture {
    flac_at(&dir.path(name), rate, bits, channels, frames)
}

/// Like `flac`, at any path (parent directories must exist).
pub fn flac_at(path: &Path, rate: u32, bits: u16, channels: u16, frames: usize) -> Fixture {
    assert_eq!(frames % BLOCK, 0, "fixtures must be whole blocks");
    let raw = noise(
        rate ^ u32::from(bits) ^ frames as u32,
        bits,
        frames * channels as usize,
    );
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .expect("encoder config");
    let source = flacenc::source::MemSource::from_samples(
        &raw,
        channels as usize,
        bits as usize,
        rate as usize,
    );
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .expect("encoding fixture");
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream.write(&mut sink).expect("serialising fixture");
    std::fs::write(path, sink.as_slice()).unwrap();
    Fixture {
        path: path.to_path_buf(),
        rate,
        bits,
        channels,
        expected: raw.iter().map(|s| s << (32 - u32::from(bits))).collect(),
    }
}

pub fn write_bytes(dir: &Dir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.path(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// A sink that takes audio at real-time speed, like a DAC, and discards it:
/// lets tests run the real player thread with realistic timing and no
/// hardware.
#[derive(Default)]
pub struct PacedSink {
    format: Option<StreamFormat>,
}

impl Sink for PacedSink {
    fn ensure(&mut self, src: StreamFormat) -> Result<StreamFormat> {
        self.format = Some(src);
        Ok(src)
    }
    fn format(&self) -> Option<StreamFormat> {
        self.format
    }
    fn write(&mut self, samples: &[i32]) -> Result<()> {
        if let Some(f) = self.format {
            let frames = samples.len() as u64 / u64::from(f.channels.max(1));
            std::thread::sleep(Duration::from_micros(
                frames * 1_000_000 / u64::from(f.rate.max(1)),
            ));
        }
        Ok(())
    }
    fn delay(&self) -> u64 {
        0
    }
    fn discard(&mut self) -> Result<()> {
        Ok(())
    }
    fn close(&mut self) {
        self.format = None;
    }
    fn stop(&mut self) {
        self.format = None;
    }
}
