#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use orbisync_realtime::{check_message_size, check_message_size_for_envelope, decode_envelope};

const NORMAL_MESSAGE_LIMIT: u64 = 16_384;
const TRANSPORT_MESSAGE_LIMIT: u64 = 65_536;

#[derive(Arbitrary, Debug)]
struct WebSocketFrame<'a> {
    binary: bool,
    payload: &'a [u8],
}

fuzz_target!(|frame: WebSocketFrame<'_>| {
    if !frame.binary {
        return;
    }

    let _ = check_message_size(frame.payload, TRANSPORT_MESSAGE_LIMIT);
    if let Ok(envelope) = decode_envelope(frame.payload) {
        let _ = check_message_size_for_envelope(
            frame.payload,
            &envelope,
            NORMAL_MESSAGE_LIMIT,
            TRANSPORT_MESSAGE_LIMIT,
        );
    }
});
