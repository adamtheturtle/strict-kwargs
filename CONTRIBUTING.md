# Contributing

## Property testing

The Rust unit tests use [proptest](https://github.com/proptest-rs/proptest) to generate inputs and shrink failures.
Run them with `cargo test --locked --lib property_tests`.
They also run in the normal CI test suite, with 256 cases per property by default.
For a longer local run, set `PROPTEST_CASES=4096`.
Commit generated `proptest-regressions` files when a failure is fixed so the minimal failing input remains covered.

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

## Unused dependencies

CI checks for unused Rust dependencies with [cargo-machete](https://github.com/bnjbvr/cargo-machete) 0.9.2.
Run the same check locally:

```console
cargo install --locked cargo-machete --version 0.9.2
cargo machete
```

Review each finding before removing a dependency, including dependencies used by macros or generated code.

## Minimum Rust version

The supported minimum Rust version is 1.92, matching the parser dependency.
CI verifies the `Cargo.toml` requirement on Linux, macOS, and Windows with [cargo-msrv](https://github.com/foresterre/cargo-msrv) 0.19.3.

```console
cargo install --locked cargo-msrv --version 0.19.3
cargo msrv verify --no-log -- cargo check --locked --all-targets --all-features
```

When dependencies require a newer compiler, update the declared minimum and verify it with this command.
