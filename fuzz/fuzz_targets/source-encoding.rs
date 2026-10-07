//! Fuzz byte-preserving Python source encoding round trips.

#![no_main]

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    strict_kwargs::fuzzing::source_encoding(bytes);
});
