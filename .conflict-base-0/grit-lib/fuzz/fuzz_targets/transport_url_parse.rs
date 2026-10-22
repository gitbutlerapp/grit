#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    grit_lib::fuzz::transport_url_parse(data);
});
