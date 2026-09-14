#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
module="${1:-restore}"
if [[ "$module" != "restore" ]]; then
    echo "error: unknown verification module '$module' (available: restore)" >&2
    exit 2
fi

"$repo_root/verification/tools/install-verus.sh"
"$repo_root/verification/tools/install-verus-source.sh"
verus_root="$(dirname "$("$repo_root/verification/tools/find-verus.sh")")"
mkdir -p "$repo_root/target/verus"
(
    cd "$repo_root"
    PATH="$verus_root:$PATH" cargo verus verify -p openvmm_core \
        --fwd-verus-args-to roots -- \
        --verify-only-module worker::dispatch \
        --verify-function 'InitializedVm::load' \
        --no-cheating \
        --no-lifetime \
        --multiple-errors 20 \
        --num-threads 1 \
        --triggers-mode silent
) 2>&1 | tee "$repo_root/target/verus/restore.log"
