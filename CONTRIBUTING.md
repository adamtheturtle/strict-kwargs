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
