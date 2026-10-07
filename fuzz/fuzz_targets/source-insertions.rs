//! Fuzz insertions at Unicode character boundaries.

#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let Ok(payload) = std::str::from_utf8(&data[4..]) else {
        return;
    };
    let mut parts = payload.splitn(3, '\0');
    let source = parts.next().unwrap_or_default();
    let first = parts.next().unwrap_or_default().to_owned();
    let second = parts.next().unwrap_or_default().to_owned();
    let insertions = [
        (u16::from_le_bytes([data[0], data[1]]), first),
        (u16::from_le_bytes([data[2], data[3]]), second),
    ];
    strict_kwargs::fuzzing::source_insertions(source, &insertions);
});
