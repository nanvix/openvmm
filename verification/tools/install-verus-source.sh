#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version="$(tr -d '[:space:]' < "$repo_root/verification/verus-version")"
source_root="$repo_root/toolchain/verus-src"
stamp="$source_root/.argus-verus-version"

if [[ -f "$stamp" ]] \
    && [[ "$(tr -d '[:space:]' < "$stamp")" == "$version" ]] \
    && [[ -f "$source_root/source/vstd/vstd.rs" ]]; then
    echo "Verus source $version is already installed"
    exit 0
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

git clone --quiet --depth 1 --branch "release/$version" \
    https://github.com/verus-lang/verus.git "$tmp_dir/verus-src"

mkdir -p "$source_root"
find "$source_root" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
cp -a "$tmp_dir/verus-src"/. "$source_root/"
printf '%s\n' "$version" > "$stamp"
echo "Installed Verus source $version at $source_root"
