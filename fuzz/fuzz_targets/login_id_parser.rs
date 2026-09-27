#![no_main]

use libfuzzer_sys::fuzz_target;
use orbisync_domain::LoginId;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = std::str::from_utf8(data) else {
        return;
    };
    let _ = LoginId::new(value);
});
