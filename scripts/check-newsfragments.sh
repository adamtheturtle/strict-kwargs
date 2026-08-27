#!/bin/sh
# Fail if `towncrier` would drop the news fragments on the floor.
#
# Supplying `type` in `pyproject.toml` replaces towncrier's built-in types
# rather than adding to them, so a fragment suffix that is not listed there
# is silently ignored: `towncrier build` renders "No significant changes"
# and leaves the files in place. That shipped an empty changelog for every
# release between the towncrier switch and issue #1259.
set -eu

fragments=$(find newsfragments -name '*.rst' | wc -l | tr -d ' ')
if [ "$fragments" -eq 0 ]; then
    exit 0
fi

# Match towncrier's marker as a whole line: a fragment may legitimately
# mention the phrase in its own prose.
notes=$(uv run --extra=release towncrier build --draft --version 0.0.0 2>/dev/null)
if printf '%s\n' "$notes" | grep -qx 'No significant changes\.'; then
    echo "towncrier ignored all $fragments news fragment(s) in newsfragments/." >&2
    echo "Every fragment suffix must be listed under [tool.towncrier] type." >&2
    exit 1
fi
