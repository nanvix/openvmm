# Native local MSHV snapshot harness

This harness boots an actual Linux microVM, takes an untiered/blockless snapshot
through the guest's normal `nvx-snapshot` PIO operation, waits for the source to
exit, then launches **two separate restore processes** from that same snapshot.
It does not use the removed `phase_2_snapshot_restore` test selector,
`phase2_snapshot_bench`, the old instrumentation patch, or any model/API calls.
It changes no product source.

## Run inside the approved bounded container

All paths must be backed by host `/mnt/data`. Native container aliases `/work`
and `/cache` are accepted for outputs; `/source`, `/sources`, `/seed`, `/harness`
and `/fixtures` are also accepted for read-only inputs. The host launcher must hold
`/mnt/data/openvmm-verification/.host.lock` for the entire operation.
`resource_check.py` fails closed unless running in Docker as uid1001/gid1003,
supplementary group998, `/dev/mshv` accessible, read-only rootfs, no Docker socket,
exact26GiB memory, zero additional swap (26GiB total),6 CPUs and1024 PIDs.
Do not run Rust or the VM directly on the host.

```sh
bash /mnt/data/openvmm-verification/native-ci/harness/build.sh \
  "$PRIVATE_SOURCE" "$NEW_NATIVE_TARGET" "$UNIQUE_BUILD_OUTPUT"

bash /mnt/data/openvmm-verification/native-ci/harness/run.sh \
  --source "$PRIVATE_SOURCE" \
  --binary "$NEW_NATIVE_TARGET/debug/openvmm" \
  --output "$RUNS_PARENT" \
  --kernel /mnt/data/nvx/build/vmlinux \
  --initrd /mnt/data/nvx/build/initramfs.cpio.gz
```

`build.sh` uses native Rust1.95, a locked MSHV-only OpenVMM build, and four build
jobs. An explicitly set `PROTOC` must name an executable file; its resolved path
and version are retained as `protoc.path` and `protoc.version`. Empty or invalid
overrides fail without restoring packages. Only when `PROTOC` is unset and the
packaged executable is missing does it invoke
`cargo xflowey restore-packages --no-compat-igvm`, with a900-second timeout.
The current native image supplies the pinned external compiler, avoiding the
package symlinks rejected by Specula source isolation. The OpenVMM build has
an1800-second timeout. Cargo home and target must be writable native-private
caches. Each build output directory must be new.

`run.py` always creates a new timestamp/UUID run directory, preserving failures,
logs, generated initrd, source diff, identities, source snapshot and hashes.
The per-VM deadline defaults to120 seconds, is configurable with
`--process-timeout` (maximum900), and kills only its own process group on timeout.
There is no automatic cleanup. No KVM fallback exists.
SIGTERM/SIGINT latch cancellation, stop the active owned child process group
with SIGKILL, and reap the child before recording a failed receipt and exiting
nonzero. Interrupted evidence includes `interrupted: true` and
`termination_signal`. Already-reaped children are never signalled; cancellation
is also checked before launches and during the delay between restores.

## Copyable Specula output contract

Copy this entire directory into the current Specula output, for example
`cp -a /harness "$SPECULA_OUTPUT/harness"`. The runtime files use only paths
relative to their own directory; they do not access the original supplied
harness, private validation clones, historical evidence, or a pinned source SHA.
The caller supplies the **current instrumented private source and its binary**.
Instrumentation environment variables are inherited unchanged.

Exact invocation, after mounting/providing the pinned guest fixtures:

```sh
bash "$SPECULA_OUTPUT/harness/build.sh" \
  "$SPECULA_SOURCE" "$CARGO_TARGET_DIR" "$SPECULA_OUTPUT/build-unique"

bash "$SPECULA_OUTPUT/harness/run.sh" \
  --source "$SPECULA_SOURCE" \
  --binary "$CARGO_TARGET_DIR/debug/openvmm" \
  --output "$SPECULA_OUTPUT/native-evidence" \
  --kernel /fixtures/vmlinux \
  --initrd /fixtures/initramfs.cpio.gz \
  --json > "$SPECULA_OUTPUT/native-run-result.json"
```

Use a new build output name if retrying; preserve the same target/cache.
`run.sh` never builds or silently substitutes another binary. After argument
validation it emits one final JSON receipt when `--json` is selected:

```json
{
  "schema": "native-openvmm-run-result-v1",
  "status": "passed",
  "run_id": "20260916T051211Z-627cb5f683d4",
  "backend": "mshv",
  "evidence": "/work/current-output/native-evidence/RUN_ID/evidence.json"
}
```

The example's receipt path is illustrative, not archived-run evidence.
Exit0 means all VM acceptance checks passed; exit1 means a runtime check failed,
with a retained failed evidence record. Invalid CLI arguments/preflight paths
can fail before creating a run or receipt. See `result.schema.json` for the
receipt and `evidence.schema.json` for the retained evidence record. **Neither
is a native Specula CI verdict/publication receipt or an implementation trace.**
Specula must separately generate fresh implementation traces, replay them and
complete its own checking/report artifacts.

The host launcher must actually supply `/harness` and `/fixtures`, or place their
contents beneath `/work` and adjust the explicit paths. Merely naming these
paths in guidance does not mount them.

This copy contract was executed successfully with the new native runtime image
`sha256:c48aaf4954c1ab1258e4b9560cefcc88bc77ed301f267de786ef4e85d0a5816b`,
using its shell entrypoint (no agent/model calls), a copied harness under `/work`,
the cached A binary, and `/fixtures` inputs. It produced another genuine MSHV
capture plus two successful restores:

* `../harness-work/specula-copy-output/native-run-result.json`
* `../harness-work/specula-copy-output/native-evidence/20260916T051959Z-33ae14249214/evidence.json`
* `../harness-work/specula-copy-output/schema-validation.json`

Both receipt and evidence validated against the supplied Draft2020-12 schemas
using the runtime's existing `jsonschema4.25.1`; no validator was installed.
The private A clone's full reachable history also reports zero missing objects,
so no hydration fetch was required.

## Evidence and acceptance

`evidence.json` is a summary of actual observations, **not a model trace**.
Every child has separate `.stdout`, `.stderr` and `.process.json` files.
Success requires:

* source exit0, one boot/request, no post-snapshot continuation;
* each restore exit37 and exactly one continuation, with no cold-boot marker;
* the snapshotted guest `/run/private-marker` initially clean in each restore,
  then successfully overwritten, plus16MiB of private guest RAM writes;
* a different guest-computed SHA256 of the actual64-byte restore entropy in
  each restore (not a claim that Linux's RNG has been reseeded);
* every source snapshot artifact retaining the same size/SHA256 after each
  restore.

The supplied guest fixture pins default to:

* kernel: `b2fdef133b4d75ea093abb69e97012eef0f5603b03a6157c5736d3afa1270cca`
* base initrd: `68aa21364eff8cd16f8cf1fb303172f46764a20d78580bbc57a95c18f4747faa`

The derivative initrd replaces only `/init`, preserving other newc entries
without extracting them. `guest-init.sh` derives the normal snapshot/timer/clock/
entropy/private-write workload from the legacy MIT-licensed
`vmm_tests/vmm_tests/tests/tests/x86_64/microvm.rs` phase2 scenario. It deliberately
bypasses the fixture's unrelated host-mount/network/sandbox init behavior.
Both base and derivative identities and the exact replacement script are saved.
For other explicitly chosen fixtures, pass their expected `--kernel-sha256`
and `--initrd-sha256`; do not silently relabel fixture incompatibilities.

Observed guest clock lines report integer-second wall/uptime and `/proc` CPU
ticks. Host elapsed nanoseconds are process-duration measurements. Neither is
an internal model clock. No guest generation counter, VM clock advancement,
boundary event, or lifecycle transition is synthesized or inferred as a trace.
Specula must instrument the actual implementation for its future trace schema.
Source SHA, source diff/status and executable SHA are recorded independently;
when supplying an externally built binary, retain its build provenance too.
Use the parent's fully materialized `native-ci/repos/source-a`, `source-b`, and
`source-current` as local clone donors, not the partial main clone. The original
validated private A clone already has all2,972 A blobs; its origin was retargeted
to `source-a` without fetching, changing HEAD, or rebuilding.

## A/B/C scope and supplemental device tests

There is no source HEAD pin in the executable harness. Initial validation uses
clean native A `df4da6d4062aa2c99ad8920459389c46bcd085c5`; the same CLI is intended
for B `6be988cb0f9739438f578094cf911dad85ca4a41` and native main. A is B's direct
parent. B refreshes completed `MicrovmSnapshotRequest` state on PIO writes,
retaining the registered poll waker and release/completion ordering. This is
a relevant semantic change, but one captured request per VM does **not**
deterministically exercise the race between a second PIO request and the poll
task. Do not claim the full VM scenario alone distinguishes A and B.

Supplement with existing deterministic chipset tests:

```sh
bash /mnt/data/openvmm-verification/native-ci/harness/component-tests.sh \
  "$PRIVATE_SOURCE" "$NEW_NATIVE_TARGET" "$UNIQUE_TEST_OUTPUT"
```

This selects existing `snapshot_request` tests using nextest's `agent` profile,
falling back to `cargo test` only if nextest is missing. A has fewer tests; B adds
completed/closed transaction reuse, early release, wakeup and stop/reset
coverage. Supplemental device tests are never substituted for full VM evidence.
The Cargo fallback additionally requires complete successful libtest summaries
and at least one observed passing test name matching `snapshot_request`; its
`test-count.json` records that count. Empty, ignored-only, unrelated-only and
incomplete output are rejected even if Cargo exits zero. Nextest's existing
fail-on-empty behavior is unchanged.
Network, control broker, sandbox block layers and clone/resume tier gates remain
outside this first harness scope.

For the **same deterministic observational workload on both revisions**, use:

```sh
bash /mnt/data/openvmm-verification/native-ci/harness/observe-reuse.sh \
  "$PRIVATE_SOURCE" "$NEW_NATIVE_TARGET" "$UNIQUE_TEST_OUTPUT"
```

Unlike `component-tests.sh`, this installs the identical
`reuse-observation.rs` as a new integration-test file under the explicitly
provided **private** source clone. It does not change the implementation or
dependencies, does not overwrite a different existing file, and retains the
test overlay and provenance. Never pass an input donor or shared source here.
If the clone lacks packages, `PROTOC` can point to the already restored native
packaged executable; no duplicate package restore is needed for this test.

The test releases/completes one real `MicrovmSnapshotRequest`, then performs
another normal PIO write before the next explicit poll. It prints the actual
PIO acceptance/coalescing and notification booleans, permitting either coherent
revision behavior and checking eventual usability. These are **observations,
not model trace records**. Specula can instrument the implementation exercised
by this test separately. This demonstrates the already-known A→B lifecycle
change; it is not a newly discovered defect.

The identical observational test has now passed on both revisions:

* A: `../harness-work/component-observations/A-2/observation.json` —
  `accepted=false`, `coalesced=true`, `notified=false`.
* B: `../harness-work/component-observations/B-1/observation.json` —
  `accepted=true`, `coalesced=false`, `notified=true`.

Each is backed by retained native nextest logs and cgroup evidence. The initial
offline A attempt (`A-1`) failed because nextest's workspace metadata requested
an uncached `openssl-src` crate. Only after that missing-dependency failure was
the bounded retry allowed to download it. No model/API call or product fix was
involved. `summarize-reuse.py` extracts only the actual observation line.

## Verified local execution

Clean A was built and this complete VM scenario passed on2026-09-16:

* build provenance:
  `../harness-work/builds/A-20260916-1/`
* VM evidence:
  `../harness-work/runs/20260916T051211Z-627cb5f683d4/evidence.json`
* runtime Docker inspection:
  `../harness-work/run-A-container-inspect.json`
* binary SHA256:
  `45e4f50407e12a522eeeecc062f6b16572b7b5ecda9e24d9c837ecbecf73f172`

The clean build used the authorized pre-existing image
`sha256:e25716b6504a8515f72bcdae7c853e2471719c771c147c83df474ef47efb1f46`
with a shell entrypoint override, never its agent entrypoint. The VM container
had networking disabled. Recorded memory peaks were6,754,615,296 bytes for
the build and228,511,744 bytes for the VM scenario; both recorded zero OOM events.
Both source and fixture originals remained unchanged.

**Timing limit observed, not hidden:** both restores reported actual wall and
uptime deltas of0 seconds and a process CPU delta of1 tick, despite the requested
3-second host delay; each restore process lasted approximately4.1 seconds.
The armed timer completed, but this run does **not** establish host-downtime
compensation or the old phase2 test's clock/timer assertions. No product fix or
synthetic expected time was introduced. This result establishes the capture/
continuation/private-memory/artifact-reuse checks listed above. B/C full VM
executions and the pre-existing chipset test selection have not yet been run
by this harness.

## Harmless harness regression tests

Inside the same bounded container, with a persistent scratch directory:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s /harness/tests -v
```

These tests use sleeping Python children, explicit mock fixtures and fake Cargo
output. They perform no Rust build, VM boot or model call. They retain temporary
files beneath `TMPDIR`, including failed mock cancellation receipts; those
receipts are regression-test artifacts, not real MSHV evidence.
