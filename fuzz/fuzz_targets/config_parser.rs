#![no_main]

use libfuzzer_sys::fuzz_target;
use orbisync_config::Config;

fuzz_target!(|data: &[u8]| {
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(table) = raw.parse::<toml::Table>() else {
        return;
    };

    let mut config = Config::default();
    for (key, value) in table {
        if let Some(value) = value.as_str() {
            let _ = config.apply(&key, value);
        }
    }
    let _ = config.validate();
});
