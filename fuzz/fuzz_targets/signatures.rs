//! Fuzz positional argument limits for generated signatures.

#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let kinds = &data[..data.len().min(32)];
    strict_kwargs::fuzzing::signatures(kinds, &String::from_utf8_lossy(data));
    for fullname in [
        "C.__call__",
        "C.__init__",
        "C.__new__",
        "C.__get__",
        "C.__set__",
        "functools.cached_property.__get__",
    ] {
        strict_kwargs::fuzzing::signatures(kinds, fullname);
    }
});
