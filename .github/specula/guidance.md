# Local native CI: Pedro microVM snapshot and restore

## Objective and source identity

Establish a source-faithful, reusable native Specula CI model of Pedro's
guest-requested microVM snapshot transaction in the new `nanvix/openvmm` fork.
Then maintain that model through actual descendant source changes. This is
not upstream OpenVMM's interactive REPL save/restore and not whole-VMM analysis.

The parent supplies a clean, read-only ordinary Git checkout. Use the source
identity in this run's CI inputs, not a path name or an assumed branch tip.
Instrument only Specula's private source copy. The reference local sequence is:

- A: `1107ec002603e262a23e5437235b81cd8bcbb377`.
- B: `0b15589c7ee0bea4b35436255c33ea8353f0a178`, A's direct child: `chipset: reuse completed microVM snapshot requests`.
- C: `0bc357bbcf3a654b63dfb51f1103c5751bf3d31f`, main selected on 2026-09-20; B is its ancestor.

These are the commits on current main history after the upstream rebase. Retained seed artifacts were generated at historical A `df4da6d4062aa2c99ad8920459389c46bcd085c5`, whose corresponding historical child was `6be988cb0f9739438f578094cf911dad85ca4a41`. The old and current source identities are not interchangeable. Reuse the retained analysis/model/harness as input to a supported current-source initialization; do not claim old traces were produced by a rebased revision. The scoped A-to-B device behavior change is the same known source-history change.

This is an experiment over existing upstream revisions, not a request to
implement a product fix. A defect already repaired in B is an existing known
source-history defect, not a newly discovered OpenVMM bug.

The old hardfork baseline at `521647e` is not an ancestor baseline for this
new repository. Do not import old acceptance, old traces, or a prior pass as
current evidence. Optional historical assets are only provisional references.

## Scope: one coherent transaction model

Prioritize semantic depth within these connected mechanisms:

1. `MicrovmSnapshotRequest` PIO request, duplicate coalescing, deferred write,
   release, write-completion notification, transaction completion, and pending
   request reuse. Include the independence of device polling and the next PIO
   write. Cover ordinary stop/reset behavior at this interface.
2. Worker/controller snapshot boundary, stop/save ordering, staged publication,
   source capture-and-exit, and ordinary no-destination or pre-commit recovery.
3. Manifest-driven new-process restore with private guest memory. For the
   explicitly supported reusable snapshot mode, one committed artifact can
   feed two independent restores without mutation by guest writes.
4. Boundary release and guest continuation must be consistent with committed
   capture or an ordinary resumed source. Preserve the distinction between
   releasing a deferred write and completing the entire transaction.

Starting points, relative to the actual source:

- `vm/devices/chipset/src/microvm.rs`, especially `MicrovmSnapshotRequest`.
- `openvmm/openvmm_entry/src/vm_controller.rs`.
- `openvmm/openvmm_core/src/worker/dispatch.rs`.
- `openvmm/openvmm_defs/src/rpc.rs`.
- `openvmm/openvmm_helpers/src/snapshot.rs` and `shared_memory.rs`.
- `openvmm/openvmm_entry/src/lib.rs`.
- `Guide/src/user_guide/openvmm/snapshots.md`.

Consult other files only for these interfaces and concrete behavior. Use small
finite bounds: one vCPU, at most two requests/restores, one snapshot generation,
and abstract RAM identity/private mutation. Do not model CPU instructions,
byte arrays, entire hardware timers, full filesystem namespaces, or whole
virtio protocols as state variables. Keep resource and timing interfaces
explicit instead of claiming those abstractions prove the hardware.

The A-to-B delta changes the request-lifecycle implementation itself. Release
runs may target later descendants: use their frozen input identity. Determine
the incremental disposition from the actual source and model; do not merely
rename the old verdict or declare model change in advance. Preserve unresolved
findings and perform the required current applicability/confirmation work.

Record PIO invocation/return boundaries and whether polling originated in
poll_device or within io_write. The controlled completed-without-poll scenario
must not insert an unobserved device poll. Record accepted/coalesced/notified
outcomes from the actual API, not from a version-dependent expected result.
When comparing the old model against a new trace, distinguish semantic
incompatibility from parser/schema incompatibility. Reconcile model-review
concerns against the current source/model before claiming acceptance.

## Contracts and explicit exclusions

Derive contracts from the selected source and its current documentation.
New snapshot tiers distinguish reusable clones from single-use instance
checkpoints. The real first scenario intentionally uses the compatible
untiered reusable microVM mode. Do not generalize its repeatability to
`instance-checkpoint`, erase a `resume.claim`, or bypass a restore gate.

Tiered sandbox resources, control-session broker/framing, network, virtio-fs,
hotplug, Windows/WHP, KVM, OpenHCL, and host REPL save/restore are outside this
initial scope. On later updates, explicitly assess whether changes to these
areas affect the in-scope interfaces before excluding them.

Clock/counter and entropy behavior may be observed in real guest output, but
this transaction model does not establish exact hardware clock post-state or
cryptographic freshness. Host clocks remain live. Never populate an observed
clock/timer field using a shadow model's predicted arithmetic.

No hostile guests, exploitation, adversarial filesystem manipulation, fuzzing,
or unrelated vulnerability search. Normal correctness counterexamples may be
confirmed with ordinary component tests or the normal VM scenario. Report
their actual impact and limits; do not label an unconfirmed mismatch a security
vulnerability. If tracker access is unavailable, novelty is unknown unless
provided source history establishes that a finding is already known.

## Real execution and trace requirements

The supplied `/harness` directory contains a parent-maintained normal scenario
and usage instructions. Reuse it rather than rebuilding a VM test framework.
It is a bootstrap aid, not automatically accepted trace evidence. Copy or
adapt the needed harness into this run's output, retaining provenance and
preserving the original. Its implementation inputs must be the current
private source/binary, not an earlier run's binary.

Read `/harness/README.md`. The copyable bootstrap entry points are:

```text
bash /harness/build.sh PRIVATE_SOURCE CARGO_TARGET_DIR UNIQUE_BUILD_OUTPUT
bash /harness/run.sh --source PRIVATE_SOURCE \
  --binary CARGO_TARGET_DIR/debug/openvmm --output UNIQUE_EVIDENCE_PARENT \
  --kernel /fixtures/vmlinux --initrd /fixtures/initramfs.cpio.gz --json
bash /harness/observe-reuse.sh PRIVATE_SOURCE CARGO_TARGET_DIR UNIQUE_TEST_OUTPUT
```

These bootstrap receipts are not model traces. Build the run's own reusable
`harness/run.sh` to arrange the actual instrumentation, fresh component/VM
traces and replay inputs. Keep any copied bootstrap implementation under a
distinct subdirectory if needed, rather than recursively invoking itself.
Use the shared `/cache` Cargo target and registry across retries, with unique
evidence/build-log directories. Read the actual helper before invoking it.

The same observational Rust test supplied in `reuse-observation.rs` exercises
the real device API on both versions. Bootstrap observations recorded A
coalescing the completed-without-poll second write and B accepting it. That
is evidence of the already-known source delta, not a substituted trace or an
instruction to force a verdict. Re-execute on this run's private source and
instrument the underlying implementation for model correspondence.

The old `phase_2_snapshot_restore` integration-test selector is absent in the
new repository. Do not invoke an empty filter and count exit zero as coverage.
The `phase2_snapshot_bench` example only measures host-side storage foundations;
it is not a real VM snapshot/restore test.

Required current-run evidence:

- Real `/dev/mshv` capture and source exit, followed by two independent restores
  of the supported reusable snapshot, with fresh guest continuation evidence.
- Source and guest-fixture identities, binary identity, unique run directory,
  process exit outcomes, and unchanged snapshot artifact hashes.
- Actual implementation-boundary instrumentation for the transaction model.
  Component tests supplement the device lifecycle interleavings that a
  nondeterministic whole-VM run does not reliably exercise.
- Nonzero executed component tests, fresh nonempty traces, actual replay
  completion and model-checking evidence for the selected finite profiles.

Emit events at actual coherent source boundaries. Notifications must not let
a dependent consumer event overtake its prerequisite trace event. Trace phase
counters are derived observations, not proof of unobserved data post-state.
Never manufacture traces from the expected model or replay archived traces
as a substitute for executing this revision.

The uninstrumented A bootstrap's integer-second guest wall/uptime deltas were
zero despite a three-second host wait. Its successful continuation and private
memory checks do not establish downtime compensation. Preserve this limitation
unless new, correctly anchored observations resolve it.

Run existing deterministic request-device tests where available, adapting
test-only instrumentation if needed. A normal focused driver may call the
actual Rust device API to expose pending/completion/reuse ordering; it must
not be a reimplementation or simulator of that API. Do not weaken expected
product behavior to make a failing scenario pass.

For `NO_MODEL_CHANGE`, still run the current implementation and replay fresh
traces. Record the precise evidence reused and why it remains applicable.
For changed semantics, maintain a complete current reference and the native
incremental update-focused artifacts; do not put replacement behavior only
in `Update.tla`.

## Resources and persistence

The outer runtime enforces exactly 26 GiB container memory, zero extra swap,
six CPUs and 1024 PIDs. Only one heavyweight experiment may run at a time.
Use at most four Cargo jobs. All caches, session state, build output, large
temporary files and TLC state must use the mounted `/work` or `/cache` paths.
The `/run` tmpfs is only 256 MiB; use `/work/scratch` for temporary files.
Original source, guest fixtures and supplied
harness are read-only.

For every TLC invocation use explicit small limits, normally:

```text
-m 6G -M 2G -w 4 -t 10
```

The native aggregate limit is 12 GiB/four workers. It does not resize the
TLC script's much larger defaults. Run campaigns sequentially. Direct Java
must likewise have explicit heap, direct-memory, workers and finite timeout.
Keep finite state bounds small enough for completed bounded checks rather
than repeatedly expanding until the host or budget is exhausted.

Use finite timeouts for Cargo, VM processes and tool commands. Preserve
partial models, build output, logs, traces, TLC results and sessions when a
command fails; diagnose and repair the infrastructure/model/harness in place.
Do not restart initialization, delete a CI store, or discard progress merely
because a command failed. Native resume continues the saved conversation.
Recovery is controlled by the pinned native runtime's finite budgets. Do not add manual retry loops, change providers or disable filtering. Exhausted policy recovery is a terminal failure; retain its evidence and existing execution hold.

Distinguish budget-limited exploration from exhaustive completion at the stated
finite bounds. The outer acceptance record does not turn OOM, startup failure,
empty traces, missing scenarios or interrupted replay into a pass, even if a
native report marker exists.

## Report-only product boundary

Do not fix product defects. Allowed edits are documented instrumentation,
ordinary tests/reproduction and models/harnesses matching the original
product semantics. Preserve every source diff; do not alter product behavior
to repair or conceal a counterexample. A faithfully modeled product bug may
produce a completed failing CI verdict and a reusable model.

Never push, publish an issue/PR, deploy a workflow, register a runner, create a
release, or modify any remote system. Do not read credentials or unrelated
host paths. No administrative GitHub credentials are available to this task.
Model authentication is supplied by the runtime, not by reading its secret.

Complete the native phase artifacts and verdict honestly. Keep reports concise
and link actual evidence. A native `current` publication means reusable model
state, not automatic human acceptance or a claim of bug-free implementation.
Product repair remains a separate future human-authorized task.

## Native initialization recovery and build compatibility

The first source-based initialization completed its source/history analysis
and modeling brief in run `20260916-053628-31e5`. It then stopped before a
Phase 2 conversation began because package restoration introduced absolute
symlinks into the private source tree. Native isolation correctly rejected
those links; no model or accepted baseline was produced.

When `/seed` is supplied, inspect and reuse its retained assets. It can contain the existing reference model, executable harness, analysis and historical evidence from `20260916-061854-c314`, not just the earlier analysis-only package. Adapt those assets to the selected source through native BYOM initialization instead of repeating historical archaeology. All current-source execution, trace replay, model validation, confirmation and final reporting remain required. Retained MC-1 investigation is historical evidence to assess, not a newly discovered or automatically finalized finding.

The runtime now provides the repository's pinned `libprotoc 27.1` through
`PROTOC` and `PROTOC_INCLUDE`, outside the private source tree. Cargo's
configuration intentionally permits this inherited compiler override.
Use that provided compiler for native GNU/MSHV builds and component tests.
Do not run broad `restore-packages` merely because `.packages` is absent:
it creates absolute compiler links and an unrelated musl sysroot, which the
native source-copy validator rejects even in ignored directories. The supplied
build helper honors an explicit `PROTOC`. Do not replace the compiler with a
different version or weaken the native symlink/source validation.
