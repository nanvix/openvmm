#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version="$(cat "$repo_root/verification/verus-version")"

if [[ -n "${VERUS:-}" ]]; then
    candidate="$VERUS"
elif [[ -x "$repo_root/.tools/verus/$version/verus" ]]; then
    candidate="$repo_root/.tools/verus/$version/verus"
elif command -v verus >/dev/null 2>&1; then
    candidate="$(command -v verus)"
else
    echo "error: Verus $version is unavailable" >&2
    echo "run 'make verify-setup' or set VERUS to the pinned executable" >&2
    exit 1
fi

actual="$("$candidate" --version | sed -n 's/^  Version: //p')"
if [[ "$actual" != "$version" ]]; then
    echo "error: expected Verus $version, found ${actual:-unknown} at $candidate" >&2
    exit 1
fi

printf '%s\n' "$candidate"
