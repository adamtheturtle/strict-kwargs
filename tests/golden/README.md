# Golden Baselines

## Sphinx completeness

`sphinx-completeness.tsv` is the golden subset for the ignored Sphinx
completeness test in `tests/sphinx_completeness.rs`.

The pinned checkout is:

- Repository: `https://github.com/sphinx-doc/sphinx.git`
- Ref: `cc7c6f435ad37bb12264f8118c8461b230e6830c`

Regenerate it with:

```shell
scripts/regenerate-sphinx-completeness-golden.sh
```

By default the script runs strict-kwargs over the pinned checkout three times,
with Sphinx installed editable into a temporary virtual environment, and stores
only diagnostics that appeared in every run. To reuse an existing checkout, set
`STRICT_KWARGS_SPHINX_CHECKOUT=/path/to/sphinx`; it must be at the pinned ref
above. To reuse an existing Python environment, set
`STRICT_KWARGS_SPHINX_PYTHON_ENV=/path/to/venv`. Otherwise the script creates
a venv with Python `3.13`, matching scheduled CI; set
`STRICT_KWARGS_SPHINX_PYTHON` to override the interpreter. To change the number
of runs, set `STRICT_KWARGS_SPHINX_RUNS`. To change the committed subset size,
set `STRICT_KWARGS_SPHINX_BASELINE_LIMIT`; the default is `5000`.

Run the opt-in test locally with:

```shell
cargo test --locked --test sphinx_completeness \
  pinned_sphinx_diagnostics_include_golden_subset -- --ignored --nocapture
```
