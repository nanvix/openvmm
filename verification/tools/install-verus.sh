#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version="$(cat "$repo_root/verification/verus-version")"
install_root="$repo_root/.tools/verus/$version"
archive="verus-$version-x86-linux.zip"
url="https://github.com/verus-lang/verus/releases/download/release/$version/$archive"
expected_sha256="463a304316888288d9226e7c232c355cc1dce829ef846a79f80373311dfe4d8a"

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
    echo "error: automatic installation supports Linux x86_64 only" >&2
    exit 1
fi

if [[ -x "$install_root/verus" ]]; then
    VERUS="$install_root/verus" "$repo_root/verification/tools/find-verus.sh" >/dev/null
    echo "Verus $version is already installed"
    exit 0
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
curl --fail --location --silent --show-error "$url" --output "$tmp_dir/$archive"
printf '%s  %s\n' "$expected_sha256" "$tmp_dir/$archive" | sha256sum --check
unzip -q "$tmp_dir/$archive" -d "$tmp_dir/unpacked"
distribution="$(find "$tmp_dir/unpacked" -mindepth 1 -maxdepth 1 -type d -print -quit)"
if [[ -z "$distribution" || ! -x "$distribution/verus" ]]; then
    echo "error: downloaded archive has no Verus executable" >&2
    exit 1
fi
mkdir -p "$(dirname "$install_root")"
mv "$distribution" "$install_root"
VERUS="$install_root/verus" "$repo_root/verification/tools/find-verus.sh" >/dev/null
echo "Installed Verus $version at $install_root"
