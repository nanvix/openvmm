#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version="$(tr -d '[:space:]' < "$repo_root/verification/verus-version")"
repository="$(tr -d '[:space:]' < "$repo_root/verification/verus-repository")"
revision="$(tr -d '[:space:]' < "$repo_root/verification/verus-revision")"
source_root="$repo_root/toolchain/verus-src"
stamp="$source_root/.argus-verus-revision"

if [[ -f "$stamp" ]] \
    && [[ "$(tr -d '[:space:]' < "$stamp")" == "$revision" ]] \
    && [[ -f "$source_root/source/vstd/vstd.rs" ]]; then
    echo "Verus source $revision is already installed"
    exit 0
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

git init --quiet "$tmp_dir/verus-src"
git -C "$tmp_dir/verus-src" remote add origin "$repository"
git -C "$tmp_dir/verus-src" fetch --quiet --depth 1 origin "$revision"
git -C "$tmp_dir/verus-src" checkout --quiet --detach FETCH_HEAD

if [[ -d "$source_root/.git" ]] && [[ -n "$(git -C "$source_root" status --short --untracked-files=no)" ]]; then
    echo "error: refusing to replace modified Verus source at $source_root" >&2
    exit 1
fi

rm -rf "$source_root"
mv "$tmp_dir/verus-src" "$source_root"
printf '%s\n' "$revision" > "$stamp"
echo "Installed Verus source $revision ($version) at $source_root"
