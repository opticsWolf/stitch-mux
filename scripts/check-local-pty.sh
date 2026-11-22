#!/usr/bin/env bash
# Fail unless stitch-mux resolves stitch-pty to the checked-out local crate.
set -euo pipefail

cd "$(dirname "$0")/.."
tree="$(cargo tree -p stitch-mux --color never)"
if grep -q 'stitch-pty v.* (registry+' <<<"$tree"; then
    echo "error: benchmark must use the local stitch-pty patch" >&2
    echo "$tree" | grep 'stitch-pty' >&2 || true
    exit 1
fi
grep 'stitch-pty v.* (D:\\' <<<"$tree" \
    || grep 'stitch-pty v.* (/' <<<"$tree" \
    || {
        echo "error: could not confirm a path dependency for stitch-pty" >&2
        exit 1
    }
