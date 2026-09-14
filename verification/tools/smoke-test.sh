#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
verus="$("$repo_root/verification/tools/find-verus.sh")"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

make -s -C "$repo_root" verify MODULE=restore >/dev/null
if make -s -C "$repo_root" verify MODULE=unknown >"$tmp_dir/unknown.log" 2>&1; then
    echo "error: unknown MODULE unexpectedly succeeded" >&2
    exit 1
fi
grep -Fq "unknown verification module 'unknown'" "$tmp_dir/unknown.log"

cat >"$tmp_dir/invalid.rs" <<'EOF'
use vstd::prelude::*;
verus! {
proof fn invalid() {
    assert(false);
}
}
fn main() {}
EOF
if "$verus" "$tmp_dir/invalid.rs" --no-cheating >"$tmp_dir/invalid.log" 2>&1; then
    echo "error: invalid proof unexpectedly succeeded" >&2
    exit 1
fi
grep -Fq "assertion failed" "$tmp_dir/invalid.log"
echo "verification smoke tests passed"
