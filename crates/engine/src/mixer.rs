use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::Result;

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

/// Pick the mixer for `device`. A hardware mixer needs a card to open, so a
/// card-less `hw`/`sysdefault` gets none; `control` is the default `PCM`.
pub fn choose(device: &str, setting: MixerSetting) -> MixerKind {
    let hardware = || match card(device) {
        Some(card) => MixerKind::Hardware {
            card: card.into(),
            control: "PCM".into(),
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
    }
}
