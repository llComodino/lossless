use std::time::{Duration, Instant};

use lossless_proto::PlayState;

#[derive(Clone, Copy, Debug)]
pub struct Clock {
    state: PlayState,
    position_ms: u64,
    duration_ms: Option<u64>,
    at: Instant,
}

impl Clock {
    pub fn from_event(
        state: PlayState,
        position_ms: u64,
        duration_ms: Option<u64>,
        now: Instant,
    ) -> Self {
        Self {
            state,
            position_ms,
            duration_ms,
            at: now,
        }
    }

    pub fn position(&self, now: Instant) -> u64 {
        let mut pos = self.position_ms;
        if self.state == PlayState::Playing {
            let elapsed = now.saturating_duration_since(self.at).as_millis();
            pos = pos.saturating_add(u64::try_from(elapsed).unwrap_or(u64::MAX));
        }
        self.duration_ms.map_or(pos, |d| pos.min(d))
    }

    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        if self.state != PlayState::Playing {
            return None;
        }
        let pos = self.position(now);
        if self.duration_ms.is_some_and(|d| pos >= d) {
            return None;
        }
        Some(Duration::from_millis(1000 - pos % 1000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn paused_clock_never_wakes() {
        let t = Instant::now();
        for s in [PlayState::Paused, PlayState::Stopped] {
            let c = Clock::from_event(s, 1200, Some(5000), t);
            assert_eq!(c.next_wake(t), None);
            assert_eq!(c.position(t + ms(3000)), 1200);
        }
    }

    #[test]
    fn playing_clock_wakes_on_the_next_second() {
        let t = Instant::now();
        let c = Clock::from_event(PlayState::Playing, 1200, Some(60_000), t);
        assert_eq!(c.next_wake(t), Some(ms(800)));
        assert_eq!(c.position(t + ms(300)), 1500);
        assert_eq!(c.next_wake(t + ms(300)), Some(ms(500)));
        let c = Clock::from_event(PlayState::Playing, 2000, None, t);
        assert_eq!(c.next_wake(t), Some(ms(1000)));
    }

    #[test]
    fn position_caps_at_duration() {
        let t = Instant::now();
        let c = Clock::from_event(PlayState::Playing, 4000, Some(5000), t);
        assert_eq!(c.position(t + ms(10_000)), 5000);
        assert_eq!(c.next_wake(t + ms(10_000)), None);
    }

    #[test]
    fn extreme_values_do_not_panic() {
        let t = Instant::now();
        let c = Clock::from_event(PlayState::Playing, u64::MAX, None, t + ms(5));
        assert_eq!(c.position(t), u64::MAX);
        assert!(c.next_wake(t).is_some());
    }
}
