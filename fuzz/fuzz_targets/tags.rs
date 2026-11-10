//! Arbitrary bytes through the tag/properties reader the library scan uses.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = lossless_library::read_track_from("fuzz", data);
});
