#![no_main]

use libfuzzer_sys::fuzz_target;
use orbisync_realtime::decode_envelope;

fuzz_target!(|data: &[u8]| {
    let _ = decode_envelope(data);
});
