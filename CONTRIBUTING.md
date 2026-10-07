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



## Fuzz testing

The [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) targets exercise our core routines with generated inputs.
CI runs each target for 60 seconds on pull requests and pushes, and for 10 minutes in the weekly run.
Each input has a 10-second timeout and the input size is capped at 4096 bytes.
Crashes and timeouts fail the job, which uploads failure artifacts.
Fuzz-only entry points are compiled with `cfg(fuzzing)` and do not add APIs to normal builds.

Use Cargo from rustup for the toolchain-qualified commands.
Install the pinned tool and compiler:

```console
cargo install --locked cargo-fuzz --version 0.13.2
rustup toolchain install nightly-2026-09-05 --profile minimal --component rust-src
```

The targets are `signatures`, `source-encoding`, `source-insertions`.
Use one target name in place of `TARGET` below:

```console
mkdir -p fuzz/corpus/TARGET
cp fuzz/seeds/TARGET/* fuzz/corpus/TARGET/
cargo +nightly-2026-09-05 fetch --locked --manifest-path fuzz/Cargo.toml
cargo +nightly-2026-09-05 fuzz run TARGET -- -max_total_time=60 -timeout=10 -rss_limit_mb=2048 -max_len=4096
```

The fuzz package has its own lockfile and a cargo-deny policy that also audits the fuzzing dependencies.
The policy permits NCSA because the LLVM fuzzing runtime requires it.
CI fetches its locked dependencies, builds offline, and checks that the lockfile stays unchanged.
Keep minimized failures as regression tests, and add useful starting inputs to `fuzz/seeds`.
The generated corpus and failure artifacts are ignored by Git.
Update the tool and nightly compiler pins together after validating every target.
