//! Anything a client can send. A request that decodes must survive
//! re-encoding unchanged.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lossless_proto::{Request, ServerMessage, decode, encode};

fuzz_target!(|data: &[u8]| {
    let Ok(line) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(req) = decode::<Request>(line) {
        let again = encode(&req).expect("encoding a decoded request");
        assert_eq!(decode::<Request>(&again).expect("re-decoding"), req);
    }
    let _ = decode::<ServerMessage>(line);
});
