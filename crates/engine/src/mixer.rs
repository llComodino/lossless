use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread::{self, JoinHandle};

use alsa::mixer::{MilliBel, Selem, SelemChannelId, SelemId};
use alsa::poll::{self, Descriptors, Flags};

use crate::{Error, Result};

/// Volume control. `Send` so the player thread can own it.
pub trait Mixer: Send {
    fn get(&self) -> Result<u8>;
    fn set(&mut self, percent: u8) -> Result<()>;

    /// The shared gain, only for a mixer that scales samples in software.
    /// The player applies it before `write`; every other mixer leaves the
    /// samples untouched.
    fn gain(&self) -> Option<Arc<AtomicU8>> {
        None
    }

    /// Start a thread that reports changes made outside the daemon. Only a
    /// hardware mixer has any.
    fn watch(&self, _on_change: Box<dyn Fn(u8) + Send>) -> Result<Option<JoinHandle<()>>> {
        Ok(None)
    }

    /// Get a fresh handle after the card went away and came back.
    fn reopen(&mut self) -> Result<()> {
        Ok(())
    }
}

pub struct SoftwareMixer {
    gain: Arc<AtomicU8>,
}

impl SoftwareMixer {
    pub fn new() -> Self {
        Self {
            gain: Arc::new(AtomicU8::new(100)),
        }
    }
}

impl Default for SoftwareMixer {
    fn default() -> Self {
        Self::new()
    }
}

impl Mixer for SoftwareMixer {
    fn get(&self) -> Result<u8> {
        Ok(self.gain.load(Ordering::Relaxed))
    }

    fn set(&mut self, percent: u8) -> Result<()> {
        self.gain.store(percent.min(100), Ordering::Relaxed);
        Ok(())
    }

    fn gain(&self) -> Option<Arc<AtomicU8>> {
        Some(Arc::clone(&self.gain))
    }
}

/// An ALSA simple control on `hw:<card>`. It only sets the DAC's own level:
/// samples are never touched.
pub struct HardwareMixer {
    card: String,
    control: String,
    device: String,
    mixer: alsa::mixer::Mixer,
    id: SelemId,
}

fn device_error(device: &str, reason: impl ToString) -> Error {
    Error::Device {
        device: device.into(),
        reason: reason.to_string(),
    }
}

/// Percent at `v`, linear over `lo..=hi`.
fn percent(v: i64, lo: i64, hi: i64) -> u8 {
    if hi <= lo {
        return 100;
    }
    let span = hi - lo;
    (((v.clamp(lo, hi) - lo) * 100 + span / 2) / span) as u8
}

/// The value at `p` percent of `lo..=hi`.
fn level(p: u8, lo: i64, hi: i64) -> i64 {
    let span = (hi - lo).max(0);
    lo + (span * i64::from(p) + 50) / 100
}

/// The control's playback dB range, or its raw range when it has no dB
/// information.
fn range(s: &Selem) -> (bool, i64, i64) {
    let (lo, hi) = s.get_playback_db_range();
    if lo < hi {
        (true, lo.0, hi.0)
    } else {
        let (lo, hi) = s.get_playback_volume_range();
        (false, lo, hi)
    }
}

impl HardwareMixer {
    pub fn new(card: &str, control: &str) -> Result<Self> {
        let device = format!("hw:{card}");
        let mixer =
            alsa::mixer::Mixer::new(&device, false).map_err(|e| device_error(&device, e))?;
        let m = Self {
            card: card.into(),
            control: control.into(),
            id: SelemId::new(control, 0),
            mixer,
            device,
        };
        m.selem()?;
        Ok(m)
    }

    fn selem(&self) -> Result<Selem<'_>> {
        self.mixer
            .find_selem(&self.id)
            .filter(Selem::has_playback_volume)
            .ok_or_else(|| {
                device_error(
                    &self.device,
                    format!("no playback volume control {:?}", self.control),
                )
            })
    }

    /// Block until the card reports a control change, then read the level.
    fn next_change(&self, fds: &mut [poll::pollfd]) -> Result<u8> {
        let err = |e: alsa::Error| device_error(&self.device, e);
        poll::poll(fds, -1).map_err(err)?;
        let flags = self.mixer.revents(fds).map_err(err)?;
        if flags.intersects(Flags::ERR | Flags::HUP | Flags::NVAL) {
            return Err(device_error(&self.device, "device gone"));
        }
        self.get()
    }
}

impl Mixer for HardwareMixer {
    fn get(&self) -> Result<u8> {
        let err = |e: alsa::Error| device_error(&self.device, e);
        self.mixer.handle_events().map_err(err)?;
        let s = self.selem()?;
        let (db, lo, hi) = range(&s);
        let ch = SelemChannelId::mono();
        let v = if db {
            s.get_playback_vol_db(ch).map_err(err)?.0
        } else {
            s.get_playback_volume(ch).map_err(err)?
        };
        Ok(percent(v, lo, hi))
    }

    fn set(&mut self, p: u8) -> Result<()> {
        let s = self.selem()?;
        let (db, lo, hi) = range(&s);
        let v = level(p.min(100), lo, hi);
        if db {
            s.set_playback_db_all(MilliBel(v), alsa::Round::Ceil)
        } else {
            s.set_playback_volume_all(v)
        }
        .map_err(|e| device_error(&self.device, e))
    }

    fn watch(&self, on_change: Box<dyn Fn(u8) + Send>) -> Result<Option<JoinHandle<()>>> {
        watch(&self.card, &self.control, on_change).map(Some)
    }

    fn reopen(&mut self) -> Result<()> {
        *self = Self::new(&self.card, &self.control)?;
        Ok(())
    }
}

/// Report every change of `control` on `card` until the card goes away.
/// The thread sleeps in `poll` with no timeout: nothing runs between events.
pub fn watch(
    card: &str,
    control: &str,
    on_change: impl Fn(u8) + Send + 'static,
) -> Result<JoinHandle<()>> {
    let m = HardwareMixer::new(card, control)?;
    let mut fds = m.mixer.get().map_err(|e| device_error(&m.device, e))?;
    thread::Builder::new()
        .name("mixer".into())
        .spawn(move || relay(|| m.next_change(&mut fds), on_change))
        .map_err(|e| device_error(card, e))
}

/// Pass each level read to `on_change`; the first error ends the watch. The
/// player starts a new one on its next successful `ensure`.
fn relay(mut next: impl FnMut() -> Result<u8>, on_change: impl Fn(u8)) {
    loop {
        match next() {
            Ok(p) => on_change(p),
            Err(e) => {
                log::warn!("mixer: {e}; watcher stopped");
                return;
            }
        }
    }
}

/// Scale `samples` by `(percent/100)^2`. Decoded samples are full-scale i32
/// whatever the stream depth and the factor never exceeds 1, so the result
/// cannot leave the i32 range; the clamp only makes that explicit. 100 skips
/// the multiply entirely.
pub(crate) fn apply(samples: &mut [i32], percent: u8) {
    if percent >= 100 {
        return;
    }
    let p = i64::from(percent);
    let factor = (p * p * 65536 + 5000) / 10000;
    for s in samples {
        *s = ((i64::from(*s) * factor) >> 16).clamp(i32::MIN.into(), i32::MAX.into()) as i32;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixerSetting {
    Auto,
    Hardware,
    Software,
    None,
}

impl std::str::FromStr for MixerSetting {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "hardware" => Ok(Self::Hardware),
            "software" => Ok(Self::Software),
            "none" => Ok(Self::None),
            _ => Err(format!(
                "mixer {s:?} must be one of auto, hardware, software, none"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MixerKind {
    Hardware { card: String, control: String },
    Software,
    None,
}

/// The ALSA card named by `device` (`hw:CARD=X,DEV=0`, `plughw:X,0`, ...).
fn card(device: &str) -> Option<&str> {
    let (plugin, args) = device.split_once(':')?;
    if !matches!(plugin, "hw" | "plughw" | "front" | "sysdefault") {
        return None;
    }
    let first = args.split(',').next()?;
    let card = first.strip_prefix("CARD=").unwrap_or(first);
    (!card.is_empty() && !card.contains('=')).then_some(card)
}

impl MixerKind {
    /// The mixer itself. A card or control that can't be opened leaves the
    /// daemon running without volume control.
    pub fn open(self) -> Option<Box<dyn Mixer>> {
        match self {
            Self::Software => Some(Box::new(SoftwareMixer::new())),
            Self::Hardware { card, control } => match HardwareMixer::new(&card, &control) {
                Ok(m) => Some(Box::new(m)),
                Err(e) => {
                    log::warn!("mixer: {e}; no volume control");
                    None
                }
            },
            Self::None => None,
        }
    }
}

/// Pick the mixer for `device`. A hardware mixer needs a card to open, so a
/// card-less `hw`/`sysdefault` gets none.
pub fn choose(device: &str, setting: MixerSetting, control: &str) -> MixerKind {
    let hardware = || match card(device) {
        Some(card) => MixerKind::Hardware {
            card: card.into(),
            control: control.into(),
        },
        None => MixerKind::None,
    };
    match setting {
        MixerSetting::None => MixerKind::None,
        MixerSetting::Software => MixerKind::Software,
        MixerSetting::Hardware => hardware(),
        MixerSetting::Auto => match device {
            "default" | "pipewire" | "pulse" => MixerKind::Software,
            _ => hardware(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn full_gain_is_bit_exact() {
        let src = [i32::MIN, -1, 0, 1, 12345, i32::MAX];
        let mut s = src;
        apply(&mut s, 100);
        assert_eq!(s, src);
    }

    #[test]
    fn zero_gain_is_silence() {
        let mut s = [i32::MIN, -1, 7, i32::MAX];
        apply(&mut s, 0);
        assert_eq!(s, [0; 4]);
    }

    #[test]
    fn half_gain_is_a_quarter_amplitude() {
        let mut s = [1 << 20, -(1 << 20)];
        apply(&mut s, 50);
        assert_eq!(s, [1 << 18, -(1 << 18)]);
    }

    #[test]
    fn software_mixer_stores_percent() {
        let mut m = SoftwareMixer::default();
        assert_eq!(m.get().unwrap(), 100);
        m.set(40).unwrap();
        assert_eq!(m.get().unwrap(), 40);
        assert_eq!(m.gain().unwrap().load(Ordering::Relaxed), 40);
        m.set(200).unwrap();
        assert_eq!(m.get().unwrap(), 100);
    }

    proptest! {
        #[test]
        fn gain_never_overflows(
            samples in proptest::collection::vec(any::<i32>(), 0..64),
            p in 0u8..=100,
        ) {
            let mut s = samples.clone();
            apply(&mut s, p);
            for (a, b) in s.iter().zip(&samples) {
                prop_assert!(a.unsigned_abs() <= b.unsigned_abs());
            }
        }
    }

    #[test]
    fn choose_follows_the_device() {
        let hw = |c: &str| MixerKind::Hardware {
            card: c.into(),
            control: "PCM".into(),
        };
        let choose = |d, s| choose(d, s, "PCM");
        let auto = |d| choose(d, MixerSetting::Auto);
        assert_eq!(auto("hw:CARD=PRO2,DEV=0"), hw("PRO2"));
        assert_eq!(auto("hw:1,0"), hw("1"));
        assert_eq!(auto("plughw:CARD=X"), hw("X"));
        assert_eq!(auto("front:CARD=X,DEV=0"), hw("X"));
        assert_eq!(auto("sysdefault:CARD=X"), hw("X"));
        assert_eq!(auto("front:X,0"), hw("X"));
        assert_eq!(auto("front:X"), hw("X"));
        assert_eq!(auto("sysdefault:X,0"), hw("X"));
        assert_eq!(auto("sysdefault:X"), hw("X"));
        assert_eq!(auto("hw"), MixerKind::None);
        assert_eq!(auto("sysdefault"), MixerKind::None);
        assert_eq!(auto("hw:"), MixerKind::None);
        assert_eq!(auto("front:DEV=0"), MixerKind::None);
        assert_eq!(auto("default"), MixerKind::Software);
        assert_eq!(auto("pipewire"), MixerKind::Software);
        assert_eq!(auto("pulse"), MixerKind::Software);
        assert_eq!(auto("null"), MixerKind::None);
        assert_eq!(choose("hw:X", MixerSetting::Software), MixerKind::Software);
        assert_eq!(choose("hw:X", MixerSetting::None), MixerKind::None);
        assert_eq!(choose("hw:X", MixerSetting::Hardware), hw("X"));
        assert_eq!(choose("default", MixerSetting::Hardware), MixerKind::None);
        assert_eq!(
            super::choose("hw:X", MixerSetting::Auto, "Master"),
            MixerKind::Hardware {
                card: "X".into(),
                control: "Master".into()
            }
        );
    }

    #[test]
    fn percent_is_linear_over_the_range() {
        assert_eq!(level(50, -6000, 0), -3000);
        assert_eq!(level(0, -6000, 0), -6000);
        assert_eq!(level(100, -6000, 0), 0);
        assert_eq!(percent(-3000, -6000, 0), 50);
        assert_eq!(percent(-9_999_999, -6000, 0), 0, "mute reads as 0");
        assert_eq!(percent(10, 0, 5), 100);
        assert_eq!(percent(7, 7, 7), 100, "a fixed control is full");
        assert_eq!(level(40, 7, 7), 7);
    }

    proptest! {
        #[test]
        fn percent_round_trips_on_fine_ranges(
            p in 0u8..=100,
            lo in -20_000i64..20_000,
            steps in 100i64..20_000,
        ) {
            prop_assert_eq!(percent(level(p, lo, lo + steps), lo, lo + steps), p);
        }
    }

    #[test]
    fn opening_a_missing_card_fails_as_a_device_error() {
        let err = HardwareMixer::new("NoSuchCard9", "PCM").err().unwrap();
        assert_eq!(err.code(), lossless_proto::ErrorCode::Device);
        assert!(err.to_string().starts_with("hw:NoSuchCard9: "), "{err}");
        assert!(watch("NoSuchCard9", "PCM", |_| {}).is_err());
        let kind = MixerKind::Hardware {
            card: "NoSuchCard9".into(),
            control: "PCM".into(),
        };
        assert!(kind.open().is_none());
        assert!(MixerKind::None.open().is_none());
        let mut soft = MixerKind::Software.open().unwrap();
        assert!(soft.gain().is_some());
        soft.set(10).unwrap();
        assert!(soft.watch(Box::new(|_| {})).unwrap().is_none());
        soft.reopen().unwrap();
    }

    static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

    struct Capture;

    impl log::Log for Capture {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= log::Level::Warn
        }
        fn log(&self, r: &log::Record) {
            if self.enabled(r.metadata()) {
                LOG.lock().unwrap().push(r.args().to_string());
            }
        }
        fn flush(&self) {}
    }

    #[test]
    fn the_watcher_reports_changes_and_logs_its_end() {
        let _ = log::set_logger(&Capture);
        log::set_max_level(log::LevelFilter::Warn);
        let mut reads = vec![
            Err(crate::Error::Device {
                device: "hw:Gone".into(),
                reason: "No such device".into(),
            }),
            Ok(40),
            Ok(30),
        ];
        let seen = Mutex::new(Vec::new());
        relay(|| reads.pop().unwrap(), |p| seen.lock().unwrap().push(p));
        assert_eq!(*seen.lock().unwrap(), [30, 40]);
        let log = LOG.lock().unwrap();
        assert!(
            log.iter()
                .any(|l| l == "mixer: hw:Gone: No such device; watcher stopped"),
            "{log:?}"
        );
    }

    /// Restores the control to raw 120 (0 dB on the PRO2), even if an
    /// assertion fails first.
    struct Restore(String);

    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::process::Command::new("amixer")
                .args(["-q", "-c", &self.0, "sset", "PCM", "120"])
                .status();
        }
    }

    #[test]
    #[ignore = "changes a real card's volume: set LOSSLESS_TEST_MIXER_CARD"]
    fn hardware_mixer_round_trip() {
        let card = std::env::var("LOSSLESS_TEST_MIXER_CARD").expect("LOSSLESS_TEST_MIXER_CARD");
        let _restore = Restore(card.clone());
        let mut m = HardwareMixer::new(&card, "PCM").unwrap();
        m.set(50).unwrap();
        let got = m.get().unwrap();
        assert!(got.abs_diff(50) <= 1, "read back {got}");
        let (tx, rx) = std::sync::mpsc::channel();
        let _w = watch(&card, "PCM", move |p| {
            let _ = tx.send(p);
        })
        .unwrap();
        let ok = std::process::Command::new("amixer")
            .args(["-q", "-c", &card, "sset", "PCM", "100"])
            .status()
            .unwrap();
        assert!(ok.success());
        let seen = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("no volume callback");
        assert_eq!(seen, m.get().unwrap());
        assert!(seen < 100 && seen.abs_diff(50) > 1, "saw {seen}");
    }
}
