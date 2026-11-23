use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread;

use super::map::Change;

/// Batches the MPRIS thread may fall behind by before some are dropped.
pub(crate) const CAPACITY: usize = 16;

const EVERYTHING: [Change; 5] = [
    Change::PlaybackStatus,
    Change::Metadata,
    Change::Queue,
    Change::Modes,
    Change::Volume,
];

/// Hands changes to the MPRIS thread without ever blocking the fan-out: a
/// stalled bus fills the queue, further batches are dropped, and the next
/// one delivered re-announces every property (values are read fresh).
pub(crate) struct Relay {
    tx: SyncSender<Vec<Change>>,
    lost: Arc<AtomicBool>,
}

impl Relay {
    pub(crate) fn send(&self, changes: Vec<Change>) {
        if let Err(TrySendError::Full(_)) = self.tx.try_send(changes) {
            self.lost.store(true, Ordering::Release);
        }
    }
}

pub(crate) fn spawn(mut emit: impl FnMut(&[Change]) + Send + 'static) -> Relay {
    let (tx, rx) = sync_channel::<Vec<Change>>(CAPACITY);
    let lost = Arc::new(AtomicBool::new(false));
    let dropped = Arc::clone(&lost);
    let spawned = thread::Builder::new()
        .name("mpris".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            for mut changes in rx {
                if dropped.swap(false, Ordering::AcqRel) {
                    for c in EVERYTHING {
                        if !changes.contains(&c) {
                            changes.push(c);
                        }
                    }
                }
                emit(&changes);
            }
        });
    if let Err(e) = spawned {
        log::warn!("mpris: no thread: {e}");
    }
    Relay { tx, lost }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn a_stalled_bus_never_blocks_the_sender() {
        let (gate_tx, gate) = channel::<()>();
        let (seen_tx, seen) = channel::<Vec<Change>>();
        let relay = spawn(move |c: &[Change]| {
            let _ = gate.recv();
            let _ = seen_tx.send(c.to_vec());
        });
        let t = Instant::now();
        for _ in 0..1000 {
            relay.send(vec![Change::Seeked(1)]);
        }
        assert!(
            t.elapsed() < Duration::from_millis(200),
            "{:?}",
            t.elapsed()
        );
        drop(relay);
        for _ in 0..1000 {
            let _ = gate_tx.send(());
        }
        let got: Vec<Vec<Change>> = seen.iter().collect();
        assert!(got.len() <= CAPACITY + 2, "{}", got.len());
        assert!(
            got.iter()
                .any(|c| c.contains(&Change::Metadata) && c.contains(&Change::Volume)),
            "no full resync after dropping: {got:?}"
        );
    }

    #[test]
    fn in_order_without_loss() {
        let (seen_tx, seen) = channel::<Vec<Change>>();
        let relay = spawn(move |c: &[Change]| {
            let _ = seen_tx.send(c.to_vec());
        });
        relay.send(vec![Change::PlaybackStatus]);
        relay.send(vec![Change::Seeked(5)]);
        drop(relay);
        let got: Vec<Vec<Change>> = seen.iter().collect();
        assert_eq!(got, [vec![Change::PlaybackStatus], vec![Change::Seeked(5)]]);
    }
}
