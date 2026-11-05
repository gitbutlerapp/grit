#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    grit_lib::test_support::fuzz::pack_idx_parse(data);
});
