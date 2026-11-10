use std::time::{Duration, Instant};

use alsa::pcm::{Access, Format, HwParams, PCM, State};
use alsa::{Direction, ValueOr};
use anyhow::{Context, Result, anyhow, bail};
use lossless_proto::StreamFormat;

/// How long the DAC gets between closing one stream and opening the next
/// with a different format. The DAWN PRO2 times out (and can wedge) when its
/// clock is switched right after a close.
const SETTLE: Duration = Duration::from_millis(500);
/// Retries for transient open failures (USB control timeouts, busy device).
const RETRIES: u32 = 4;
const BACKOFF: Duration = Duration::from_millis(250);

/// ALSA sample layouts the output can be opened with. Decoded samples are
/// always full-scale i32 (symphonia shifts FLAC samples up to 32 bits), so
/// packing is a plain right shift and is lossless for the source depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    S16,
    S24Packed,
    S32,
}

impl Layout {
    fn alsa(self) -> Format {
        match self {
            Layout::S16 => Format::S16LE,
            Layout::S24Packed => Format::S243LE,
            Layout::S32 => Format::S32LE,
        }
    }

    fn bits(self) -> u16 {
        match self {
            Layout::S16 => 16,
            Layout::S24Packed => 24,
            Layout::S32 => 32,
        }
    }

    fn bytes(self) -> usize {
        self.bits() as usize / 8
    }

    /// Layouts that hold `bits`-deep samples without loss, narrowest first.
    fn candidates(bits: u16) -> &'static [Layout] {
        match bits {
            0..=16 => &[Layout::S16, Layout::S24Packed, Layout::S32],
            17..=24 => &[Layout::S24Packed, Layout::S32],
            _ => &[Layout::S32],
        }
    }

    pub(crate) fn pack(self, samples: &[i32], out: &mut Vec<u8>) {
        out.clear();
        out.reserve(samples.len() * self.bytes());
        for &s in samples {
            match self {
                Layout::S16 => out.extend_from_slice(&((s >> 16) as i16).to_le_bytes()),
                Layout::S24Packed => out.extend_from_slice(&(s >> 8).to_le_bytes()[..3]),
                Layout::S32 => out.extend_from_slice(&s.to_le_bytes()),
            }
        }
    }
}

fn is_transient(e: &alsa::Error) -> bool {
    use std::io::ErrorKind::{ResourceBusy, TimedOut, WouldBlock};
    matches!(
        std::io::Error::from_raw_os_error(e.errno().abs()).kind(),
        TimedOut | ResourceBusy | WouldBlock
    )
}

fn open_once(device: &str, layout: Layout, rate: u32, channels: u16) -> alsa::Result<PCM> {
    let pcm = PCM::new(device, Direction::Playback, false)?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(layout.alsa())?;
        hwp.set_channels(channels.into())?;
        hwp.set_rate_resample(false)?;
        hwp.set_rate(rate, ValueOr::Nearest)?;
        hwp.set_buffer_time_near(500_000, ValueOr::Nearest)?;
        pcm.hw_params(&hwp)?;
    }
    Ok(pcm)
}

/// Open `device` at exactly the source rate/channels with the narrowest
/// layout that holds its depth losslessly. Resampling is refused, not
/// silently applied. Transient errors are retried with backoff on the same
/// layout; anything else means the layout is unsupported.
fn open_exact(device: &str, src: StreamFormat) -> Result<(PCM, Layout)> {
    let mut errors = Vec::new();
    for &layout in Layout::candidates(src.bits) {
        let mut attempt = 0;
        loop {
            match open_once(device, layout, src.rate, src.channels) {
                Ok(pcm) => {
                    let got = pcm.hw_params_current()?.get_rate()?;
                    if got != src.rate {
                        bail!("{device} opened at {got} Hz instead of {} Hz", src.rate);
                    }
                    return Ok((pcm, layout));
                }
                Err(e) if is_transient(&e) && attempt < RETRIES => {
                    std::thread::sleep(BACKOFF * 2u32.pow(attempt));
                    attempt += 1;
                }
                Err(e) if is_transient(&e) => {
                    bail!("{device} still failing after {RETRIES} retries: {e}")
                }
                Err(e) => {
                    errors.push(format!("{:?}: {e}", layout.alsa()));
                    break;
                }
            }
        }
    }
    Err(anyhow!(
        "{device} accepts no lossless layout for {}-bit/{} Hz/{}ch ({})",
        src.bits,
        src.rate,
        src.channels,
        errors.join("; ")
    ))
}

struct OpenStream {
    pcm: PCM,
    layout: Layout,
    format: StreamFormat,
}

impl OpenStream {
    fn fits(&self, src: StreamFormat) -> bool {
        self.format.rate == src.rate
            && self.format.channels == src.channels
            && Layout::candidates(src.bits).contains(&self.layout)
    }
}

/// Where decoded audio goes. `Output` is the ALSA implementation; tests use
/// a fake. The player is generic over it, so there is no dynamic dispatch.
pub trait Sink {
    /// Make sure the sink is open for `src`; returns the format it runs at.
    fn ensure(&mut self, src: StreamFormat) -> Result<StreamFormat>;
    fn format(&self) -> Option<StreamFormat>;
    /// Write interleaved full-scale samples, blocking until all are taken.
    fn write(&mut self, samples: &[i32]) -> Result<()>;
    /// Frames written but not yet heard.
    fn delay(&self) -> u64;
    /// Throw away buffered audio but stay open.
    fn discard(&mut self) -> Result<()>;
    /// Let buffered audio play out, then release the device.
    fn close(&mut self);
    /// Release the device immediately, discarding buffered audio.
    fn stop(&mut self);
}

/// A bit-perfect output on one ALSA device. The stream stays open while the
/// source format allows it (gapless, and no clock switch); it is reopened,
/// after `SETTLE`, only when the next source needs another format.
pub struct Output {
    device: String,
    stream: Option<OpenStream>,
    closed_at: Option<Instant>,
    scratch: Vec<u8>,
}

impl Output {
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            stream: None,
            closed_at: None,
            scratch: Vec::new(),
        }
    }
}

impl Sink for Output {
    /// Make sure the device is open for `src`; returns the format it runs at.
    fn ensure(&mut self, src: StreamFormat) -> Result<StreamFormat> {
        if let Some(s) = &self.stream
            && s.fits(src)
        {
            return Ok(s.format);
        }
        self.close();
        if let Some(since) = self.closed_at.map(|t| t.elapsed())
            && since < SETTLE
        {
            std::thread::sleep(SETTLE - since);
        }
        let (pcm, layout) = open_exact(&self.device, src)?;
        let format = StreamFormat {
            rate: src.rate,
            bits: layout.bits(),
            channels: src.channels,
        };
        self.stream = Some(OpenStream {
            pcm,
            layout,
            format,
        });
        Ok(format)
    }

    fn format(&self) -> Option<StreamFormat> {
        self.stream.as_ref().map(|s| s.format)
    }

    /// Write interleaved full-scale samples, blocking until the device takes
    /// them all.
    fn write(&mut self, samples: &[i32]) -> Result<()> {
        let s = self
            .stream
            .as_ref()
            .ok_or_else(|| anyhow!("output not open"))?;
        s.layout.pack(samples, &mut self.scratch);
        let frame_bytes = s.layout.bytes() * s.format.channels as usize;
        let io = s.pcm.io_bytes();
        let mut off = 0;
        while off < self.scratch.len() {
            match io.writei(&self.scratch[off..]) {
                Ok(frames) => off += frames * frame_bytes,
                Err(e) => s.pcm.try_recover(e, true).context("ALSA write")?,
            }
        }
        Ok(())
    }

    /// Frames written but not yet heard.
    fn delay(&self) -> u64 {
        self.stream
            .as_ref()
            .and_then(|s| s.pcm.delay().ok())
            .map_or(0, |d| d.max(0) as u64)
    }

    /// Throw away buffered audio but keep the device open (pause, seek,
    /// skip): no clock switch, so no settle delay.
    fn discard(&mut self) -> Result<()> {
        if let Some(s) = &self.stream {
            s.pcm.drop()?;
            if s.pcm.state() != State::Prepared {
                s.pcm.prepare()?;
            }
        }
        Ok(())
    }

    /// Let buffered audio play out, then release the device.
    fn close(&mut self) {
        if let Some(s) = self.stream.take() {
            let _ = s.pcm.drain();
            self.closed_at = Some(Instant::now());
        }
    }

    /// Release the device immediately, discarding buffered audio.
    fn stop(&mut self) {
        if let Some(s) = self.stream.take() {
            let _ = s.pcm.drop();
            self.closed_at = Some(Instant::now());
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::Layout;

    #[test]
    fn packing_recovers_source_samples() {
        // 24-bit source samples as symphonia hands them over (shifted by 8).
        let src24 = [0x7f_ffff_i32, -0x80_0000, 1, -1, 0x12_3456];
        let full: Vec<i32> = src24.iter().map(|s| s << 8).collect();
        let mut out = Vec::new();
        Layout::S24Packed.pack(&full, &mut out);
        let back: Vec<i32> = out
            .chunks(3)
            .map(|b| i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8)
            .collect();
        assert_eq!(back, src24);

        let src16 = [i16::MAX, i16::MIN, 1, -1];
        let full: Vec<i32> = src16.iter().map(|&s| (s as i32) << 16).collect();
        Layout::S16.pack(&full, &mut out);
        let back: Vec<i16> = out
            .chunks(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(back, src16);
    }
}
