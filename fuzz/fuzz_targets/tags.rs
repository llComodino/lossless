//! Arbitrary bytes through the tag/properties reader the library scan uses.
//! The first byte picks the extension, and so the parser (FLAC or MP3).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&pick, body)) = data.split_first() else {
        return;
    };
    let name = if pick % 2 == 0 { "fuzz.flac" } else { "fuzz.mp3" };
    let _ = lossless_library::read_track_from(name, body);
});
