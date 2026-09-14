# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

.PHONY: build test verify verify-setup verify-smoke

build:
	RUSTUP_TOOLCHAIN=1.95.0 cargo check -p openvmm_core

test:
	RUSTUP_TOOLCHAIN=1.95.0 cargo nextest run --profile agent -p openvmm_core

verify:
	@verification/tools/verify.sh "$(MODULE)"

verify-setup:
	@verification/tools/install-verus.sh
	@verification/tools/install-verus-source.sh

verify-smoke:
	@verification/tools/smoke-test.sh
