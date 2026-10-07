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
