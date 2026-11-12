//! Arbitrary state files: loading returns a result and never panics.
#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let path = std::env::temp_dir().join(format!("lossless-fuzz-state-{}", std::process::id()));
    if std::fs::write(&path, data).is_ok() {
        let _ = lossless_daemon::state::load(Path::new(&path));
    }
});
