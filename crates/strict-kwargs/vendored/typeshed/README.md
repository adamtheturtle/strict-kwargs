# Vendored typeshed

This directory contains a pinned copy of the **`stdlib/` stubs** from
[python/typeshed](https://github.com/python/typeshed), embedded into the
`strict-kwargs` binary (via `include_dir!` in `src/resolve.rs`) so builtins and
the standard library resolve offline with no Python environment.

| File / dir | Purpose |
| --- | --- |
| `stdlib/` | typeshed stdlib stubs (`.pyi`), the resolver's stdlib search root |
| `COMMIT`  | the exact upstream typeshed commit this copy was taken from |
| `LICENSE` | typeshed's license (Apache-2.0) — must ship with the vendored copy |

Only `stdlib/` is vendored. Third-party stubs come from the user's
`site-packages` at runtime (PEP 561), not from here.

## Updating

Use the script — do not hand-edit:

```bash
# Pin to the latest typeshed main:
scripts/update-typeshed.sh

# Or pin to a specific commit / tag / branch:
scripts/update-typeshed.sh 098f30ecd13f56c4cef95ed47afe281c1a317dbe
```

The script:

1. Sparse-clones `stdlib/` + `LICENSE` from typeshed at the requested ref.
2. Resolves it to a concrete commit SHA.
3. No-ops if `COMMIT` already matches (idempotent).
4. Replaces `stdlib/` and `LICENSE`, writes the new SHA to `COMMIT`.
5. Runs `cargo test` — a sync that breaks parsing or changes behavior fails
   here, before anything is committed.

Then review and commit:

```bash
git status --short crates/strict-kwargs/vendored/typeshed
git add crates/strict-kwargs/vendored/typeshed
git commit -m "Bump vendored typeshed to <short-sha>"
```

## When to update

- Periodically (e.g. quarterly) to track new stdlib APIs and signature fixes.
- When a new Python version ships and you want its stdlib surface.
- When a user reports a wrong/missing stdlib signature traced to a stale stub.

## What to check after an update

`cargo test` (run automatically by the script) is the gate. If it fails:

- **Parse errors**: typeshed adopted syntax newer than the vendored Ruff
  parser (`ruff_python_*` in `Cargo.toml`) supports. Bump the parser or pin
  typeshed to an older ref.
- **Behavior changes**: a signature's parameter *kinds* changed upstream
  (e.g. a parameter became positional-only). Update the affected expectation
  in `tests/integration.rs` if the new typeshed behavior is correct.

## Notes / caveats

- Re-exports **are** followed (e.g. `os.path` → `posixpath`, `from x import
  *`), including chains. A bump that restructures typeshed re-exports is
  handled automatically; only runtime-computed `__all__` is not.
- `sys.version_info` / `sys.platform` branches in stubs are **not** evaluated;
  all branches are indexed and treated as overloads (permissive). Updating
  typeshed does not change this behavior either.
- Size: `stdlib/` is ~5 MB / ~750 files and is embedded in the binary. A bump
  that adds many files will grow the binary accordingly.
