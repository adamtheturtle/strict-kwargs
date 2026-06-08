# Golden Baselines

## Sphinx completeness

The ignored Sphinx completeness test in `tests/sphinx_completeness.rs` stores
its expected diagnostics as an `insta` snapshot:

```text
tests/snapshots/sphinx_completeness__pinned_sphinx_diagnostics.snap
```

The snapshot is a canonicalized TSV-style required diagnostic list. Every
diagnostic in the snapshot must be observed on every run; there is no
allowed-extra baseline. Extra diagnostics are not documented separately because
the Sphinx oracle still has platform- and environment-specific resolver drift.

The pinned checkout is:

- Repository: `https://github.com/sphinx-doc/sphinx.git`
- Ref: `cc7c6f435ad37bb12264f8118c8461b230e6830c`
- Python dependencies: `tests/golden/sphinx-requirements-constraints.txt`
- `ty`: `0.0.44`

Regenerate it with:

```shell
scripts/regenerate-sphinx-completeness-golden.sh
```

By default the script runs strict-kwargs over the pinned checkout three times,
with Sphinx installed editable into a temporary virtual environment. It runs
the checker through a temporary `ty==0.0.44` wrapper so the oracle does not
drift when a newer `ty` release changes hover display details. The script sets
`STRICT_KWARGS_REGENERATE_SPHINX_GOLDEN=1` and `INSTA_UPDATE=always` to
refresh the committed snapshot directly. The Sphinx virtual environment is
installed with `tests/golden/sphinx-requirements-constraints.txt` so the oracle
does not drift when transitive dependencies such as `docutils` or `Jinja2`
change their public type surface.

To reuse an existing checkout, set
`STRICT_KWARGS_SPHINX_CHECKOUT=/path/to/sphinx`; it must be at the pinned ref
above. To reuse an existing Python environment, set
`STRICT_KWARGS_SPHINX_PYTHON_ENV=/path/to/venv`. Otherwise the script creates
a venv with Python `3.13`, matching scheduled CI; set
`STRICT_KWARGS_SPHINX_PYTHON` to override the interpreter. To intentionally
refresh the third-party dependency surface, update
`tests/golden/sphinx-requirements-constraints.txt` and regenerate the oracle in
the same change. To change the number of runs, set `STRICT_KWARGS_SPHINX_RUNS`.
To intentionally update the pinned `ty` version, set
`STRICT_KWARGS_SPHINX_TY_VERSION` while regenerating and update the version
documented here and in `tests/sphinx_completeness.rs`.

Review regenerated diffs as an oracle change, not as a blind snapshot update:

- additions are newly reported diagnostics and should be checked for false
  positives before committing
- removals are expected resolver improvements or lost coverage and should be
  explained in the change that updates the snapshot
- local-only or platform-specific diagnostics should not be added unless they
  are stable in CI too

Run the opt-in test locally with:

```shell
cargo test --locked --test sphinx_completeness \
  pinned_sphinx_diagnostics_match_golden_oracle -- --ignored --nocapture
```

## Snapshot tooling decision

`insta` is the best fit now that the oracle no longer carries an allowed-extra
set. The test still keeps the Sphinx-specific setup in Rust and shell code:
pinned checkout setup, pinned `ty`, Python environment control, multi-run
stable-diagnostic filtering, required-baseline checking, and canonicalized
diagnostic keys. `insta` handles the large golden file, regeneration,
review-oriented diffs, and optional `cargo insta review` workflow.

Other options considered were weaker for this shape:

- `expect-test` can store inline or file-based expectations, but the snapshot is
  large enough that `insta`'s `.snap.new` and `cargo insta` review workflow is
  more useful.
- `pretty_assertions` improves direct assertion diffs, but it does not provide
  snapshot storage or regeneration.
