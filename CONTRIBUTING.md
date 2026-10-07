# Contributing

## Rust visibility

CI checks Rust visibility with [Cargo Hawk](https://github.com/astral-sh/hawk) 0.1.15 and Rust 1.99.0.
Hawk includes callers in the CLI, tests, benchmarks, and doctests when checking the supporting library.

Install the [Hawk 0.1.15 release](https://github.com/astral-sh/hawk/releases/tag/0.1.15) for Linux or macOS and put both `cargo-hawk` and `cargo-hawk-driver` on `PATH`.
Install its matching Rust toolchain, then run the check with Cargo from rustup:

```console
rustup toolchain install 1.99.0
cargo +1.99.0 hawk check -D warnings -D hawk::unnecessary_crate_visibility
```

Update the Hawk version, archive checksum, and matching Rust toolchain together in CI.
The pinned compiler applies to this check only.



## Property testing

The Rust unit tests use [proptest](https://github.com/proptest-rs/proptest) to generate inputs and shrink failures.
Run them with `cargo test --locked --lib property_tests`.
They also run in the normal CI test suite, with 256 cases per property by default.
For a longer local run, set `PROPTEST_CASES=4096`.
Commit generated `proptest-regressions` files when a failure is fixed so the minimal failing input remains covered.
