//! Arbitrary device names: whatever is accepted must be harmless.
#![no_main]

use libfuzzer_sys::fuzz_target;

mod harmless;

fuzz_target!(|data: &[u8]| {
    let Ok(name) = std::str::from_utf8(data) else {
        return;
    };
    if lossless_daemon::check_device(name).is_ok() {
        assert!(harmless::device(name), "{name:?}");
    }
});
