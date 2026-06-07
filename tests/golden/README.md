# Golden Baselines

## Sphinx completeness

`sphinx-completeness.tsv` is the stable golden baseline for the ignored Sphinx
completeness test in `tests/sphinx_completeness.rs`. Every diagnostic in that
file must appear on every observed run.

`sphinx-completeness-allowed-extra.tsv` documents unstable diagnostics that
appeared during regeneration but did not appear on every run. These entries are
allowed when they appear as extras, but they are not required.

The pinned checkout is:

- Repository: `https://github.com/sphinx-doc/sphinx.git`
- Ref: `cc7c6f435ad37bb12264f8118c8461b230e6830c`
- `ty`: `0.0.44`

Regenerate it with:

```shell
scripts/regenerate-sphinx-completeness-golden.sh
```

By default the script runs strict-kwargs over the pinned checkout three times,
with Sphinx installed editable into a temporary virtual environment. It writes
diagnostics that appeared in every run to `sphinx-completeness.tsv`, and
diagnostics that appeared in at least one but not every run to
`sphinx-completeness-allowed-extra.tsv`. The script runs the checker through a
temporary `ty==0.0.44` wrapper so the oracle does not drift when a newer `ty`
release changes hover display details.

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

- additions to `sphinx-completeness.tsv` are newly stable diagnostics and should
  be checked for false positives before committing
- removals from `sphinx-completeness.tsv` are expected resolver improvements or
  lost coverage and should be explained in the change that updates the baseline
- additions to `sphinx-completeness-allowed-extra.tsv` are unstable diagnostics;
  keep them there only when the instability is understood and acceptable
- removals from `sphinx-completeness-allowed-extra.tsv` mean an unstable
  diagnostic disappeared or became stable

Run the opt-in test locally with:

```shell
cargo test --locked --test sphinx_completeness \
  pinned_sphinx_diagnostics_match_golden_oracle -- --ignored --nocapture
```
