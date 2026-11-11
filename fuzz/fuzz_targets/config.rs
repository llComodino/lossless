//! Arbitrary config files: whatever parses must be harmless.
#![no_main]

use libfuzzer_sys::fuzz_target;

mod harmless;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(cfg) = lossless_daemon::parse_config(text) {
        assert!(harmless::device(&cfg.device), "{:?}", cfg.device);
        for p in cfg.music_dir.iter().chain(&cfg.socket) {
            assert!(harmless::path(p), "{p:?}");
        }
    }
});
