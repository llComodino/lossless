//! Arbitrary bytes as an audio file: open, decode, seek, decode again.
//! The first byte picks the format hint; the decoder must never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lossless_engine::Source;

/// Bound the work per input so slow-but-valid streams don't stall the run.
const MAX_SAMPLES: usize = 1 << 22;

fuzz_target!(|data: &[u8]| {
    let Some((&hint, body)) = data.split_first() else {
        return;
    };
    let ext = match hint % 3 {
        0 => Some("flac"),
        1 => Some("mp3"),
        _ => None,
    };
    let Ok(mut src) = Source::from_bytes(body.to_vec(), ext) else {
        return;
    };
    let mut seen = 0;
    while let Ok(Some(chunk)) = src.next_chunk() {
        seen += chunk.len();
        if seen > MAX_SAMPLES {
            break;
        }
    }
    let target = u64::from(hint) * 997;
    if src.seek(target).is_ok() {
        let _ = src.next_chunk();
    }
});
