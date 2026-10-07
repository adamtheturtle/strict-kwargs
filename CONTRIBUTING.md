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

## Mutation testing

CI uses [cargo-mutants](https://mutants.rs/) to check the positional-argument signature rules in `src/signature.rs`.
The pilot runs the signature unit tests for each mutation, with a 30-second test timeout and a 180-second build timeout.
Missed mutations and timeouts fail the job, and CI uploads the results for review.
The same check also runs weekly and can be started manually.

Install the pinned tool and run the check locally:

```console
cargo install --locked cargo-mutants --version 26.2.0
cargo mutants --in-place --timeout 30 --build-timeout 180
```

The scope and test command are defined in `.cargo/mutants.toml`.
Review surviving mutations before broadening the pilot.
The tool is pinned to 26.2.0 because 27.1.0 ignores regex filters for struct-field mutations.
Update the pin after the fix for [cargo-mutants #632](https://github.com/sourcefrog/cargo-mutants/issues/632) is released.

## Minimum Rust version

The supported minimum Rust version is 1.92, matching the parser dependency.
CI verifies the `Cargo.toml` requirement on Linux, macOS, and Windows with [cargo-msrv](https://github.com/foresterre/cargo-msrv) 0.19.3.

```console
cargo install --locked cargo-msrv --version 0.19.3
cargo msrv verify --no-log -- cargo check --locked --all-targets --all-features
```

When dependencies require a newer compiler, update the declared minimum and verify it with this command.
