# Golden Baselines

## Sphinx completeness

The ignored Sphinx completeness test in `tests/sphinx_completeness.rs` stores
its expected diagnostics as an `insta` snapshot:

```text
tests/snapshots/sphinx_completeness__pinned_sphinx_diagnostics.snap
```

The snapshot is a canonicalized TSV-style diagnostic list. Every diagnostic in
the snapshot must match the observed diagnostics exactly; there is no
allowed-extra baseline. When the test is run more than once, diagnostics must
appear on every observed run before the snapshot assertion is reached.

The pinned checkout is:

- Repository: `https://github.com/sphinx-doc/sphinx.git`
- Ref: `cc7c6f435ad37bb12264f8118c8461b230e6830c`
- `ty`: `0.0.44`

Regenerate it with:

```shell
scripts/regenerate-sphinx-completeness-golden.sh
```

By default the script runs strict-kwargs over the pinned checkout three times,
with Sphinx installed editable into a temporary virtual environment. It runs
the checker through a temporary `ty==0.0.44` wrapper so the oracle does not
drift when a newer `ty` release changes hover display details. The script sets
`INSTA_UPDATE=always` and refreshes the committed snapshot directly.

To reuse an existing checkout, set
`STRICT_KWARGS_SPHINX_CHECKOUT=/path/to/sphinx`; it must be at the pinned ref
above. To reuse an existing Python environment, set
`STRICT_KWARGS_SPHINX_PYTHON_ENV=/path/to/venv`. Otherwise the script creates
a venv with Python `3.13`, matching scheduled CI; set
`STRICT_KWARGS_SPHINX_PYTHON` to override the interpreter. To change the number
of runs, set `STRICT_KWARGS_SPHINX_RUNS`. To intentionally update the pinned
`ty` version, set `STRICT_KWARGS_SPHINX_TY_VERSION` while regenerating and
update the version documented here and in `tests/sphinx_completeness.rs`.

Review regenerated diffs as an oracle change, not as a blind snapshot update:

- additions are newly reported diagnostics and should be checked for false
  positives before committing
- removals are expected resolver improvements or lost coverage and should be
  explained in the change that updates the snapshot
- any unstable diagnostics across multiple regeneration runs fail before the
  snapshot is updated

Run the opt-in test locally with:

```shell
cargo test --locked --test sphinx_completeness \
  pinned_sphinx_diagnostics_match_golden_oracle -- --ignored --nocapture
```

## Snapshot tooling decision

`insta` is the best fit now that the oracle no longer carries an allowed-extra
set. The test still keeps the Sphinx-specific setup in Rust and shell code:
pinned checkout setup, pinned `ty`, Python environment control, multi-run
stability checking, and canonicalized diagnostic keys. `insta` handles the
large golden file, regeneration, review-oriented diffs, and optional
`cargo insta review` workflow.

Other options considered were weaker for this shape:

- `expect-test` can store inline or file-based expectations, but the snapshot is
  large enough that `insta`'s `.snap.new` and `cargo insta` review workflow is
  more useful.
- `pretty_assertions` improves direct assertion diffs, but it does not provide
  snapshot storage or regeneration.
