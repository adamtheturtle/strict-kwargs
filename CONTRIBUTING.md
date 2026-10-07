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

## Dependency policy

CI checks Rust dependency advisories, licences, sources, and duplicate versions with [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) 0.20.2.
The policy in `deny.toml` includes development and platform dependencies.
Run the check locally:

```console
cargo install --locked cargo-deny --version 0.20.2
cargo deny --locked check -D warnings
```

The policy rejects unknown sources, unapproved licences, wildcard requirements, new duplicate versions, and advisory warnings.
Existing duplicate versions have exact-version exceptions with reasons in `deny.toml`.
Review those exceptions when updating dependencies and remove them when the dependency graph permits it.

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
