# Local native runtime

This repository-owned copy is used by `../release.py`. Prefer that entrypoint
for release/tag jobs: it enforces source selection, explicit initialization,
provider approval, candidate publication and curated reporting. The direct
launcher commands below describe the underlying runtime and the preserved
standalone experiment; they are not the release workflow entrypoint.

This separate runtime pins Specula 1.2.0 at `088049c5b3474340213cded2664cdb674bff1e1a`
and reuses the immutable legacy image as a toolchain base. It does not alter the
legacy controller, image, source, or evidence. It does not install a workflow.

```bash
# Requires the retained cache/protoc-27.1 package and immutable local base image.
# Run from the trusted OpenVMM checkout.
controller=.github/specula/controller
bash "$controller/build-image.sh"

python3 "$controller/run.py" \
  --work /mnt/data/openvmm-verification/native-ci/cache/runtime-probe \
  probe

# Explicit bounded model/authentication call; NOT part of ordinary probe.
python3 "$controller/run.py" \
  --work /mnt/data/openvmm-verification/native-ci/work \
  --timeout-seconds=150 --phase-timeout-seconds=120 auth-probe

python3 "$controller/run.py" \
  --work /mnt/data/openvmm-verification/native-ci/work \
  --source /mnt/data/openvmm-verification/native-ci/repos/CHOSEN-CLEAN-CLONE \
  native -- --ci-init --revision=EXACT_SHA --guidance=/work/guidance.md \
  'pedro-microvm|nanvix/openvmm|Rust|Pedro microVM snapshot/restore'

# Same store; source must already be at the intended descendant revision.
python3 "$controller/run.py" \
  --work /mnt/data/openvmm-verification/native-ci/work \
  --source /mnt/data/openvmm-verification/native-ci/repos/NEXT-CLEAN-CLONE \
  native -- --incremental --revision=EXACT_NEXT_SHA

# Resume original native conversation; no fresh-context or skip flags.
python3 "$controller/run.py" \
  --work /mnt/data/openvmm-verification/native-ci/work \
  --source /mnt/data/openvmm-verification/native-ci/repos/CHOSEN-CLEAN-CLONE \
  native -- --run-id=EXISTING_NATIVE_RUN_ID

# Explicit command mode, without any credential mounted:
python3 "$controller/run.py" \
  --work /mnt/data/openvmm-verification/native-ci/work \
  --source /mnt/data/openvmm-verification/native-ci/repos/CHOSEN-CLEAN-CLONE \
  exec -- bash -c 'test -r /dev/mshv && git -C /source rev-parse HEAD'
```

## Mapping and boundaries

- `--work` → `/work` RW; native persistent state is `/work/ci`. HOME, Copilot
  sessions, tool configuration, scratch, TLC states, and resource ledgers are
  below `/work`. Do not change this mount path between attempts.
- `--cache` → `/cache` RW (default `native-ci/cache/runtime`); language/build caches
  are here. Mount roots must be disjoint.
- `--source` → `/source` RO, from `native-ci/repos`. It must be a normal clean Git
  clone, not a worktree/submodule. Native Specula creates private source copies.
- The `native-ci/repos` directory is also mounted RO at `/sources` (override with
  `--sources` selecting a directory below it). Instead of `--source`, native
  callers can pass `--artifact=/sources/ci-a` explicitly. Resume can omit both
  artifact and `--source`: native state restores its saved private source and path.
- Optional `--harness native-ci/harness` → `/harness` RO. Use absolute host paths.
  Optional `--vmlinux FILE --initramfs FILE` mount only those two files RO at
  `/fixtures/vmlinux` and `/fixtures/initramfs.cpio.gz`; no entire NVX tree is
  mounted. `--work native-ci/harness-work` provides the same writable `/work`
  namespace for standalone harness commands. There is no `/workspace` mount.
- Optional `--seed /mnt/data/openvmm-verification/PATH` → `/seed` RO. Select it
  explicitly with native `--byom=/seed` during initialization only. Remount the
  same seed when resuming that initialization.
- Native and explicitly requested auth-probe modes mount the dedicated Copilot credential read-only
  at `/run/secrets/copilot-auth.json`. No host HOME, SSH agent, GitHub CLI
  configuration, GH/GITHUB token environment, or Docker socket is mounted.
- Every container has 26 GiB memory and 26 GiB total memory+swap, six CPUs,
  1024 PIDs, UID:GID 1001:1003, device group 998, `/dev/mshv`, cap-drop ALL,
  no-new-privileges, read-only rootfs, and a 256 MiB `/run` tmpfs.
- Both building and running acquire `/mnt/data/openvmm-verification/.host.lock`.
  This does not coordinate the independent NVX runner. Builds use a bounded,
  nonpersistent Docker builder; they do not start an idle BuildKit daemon.

The launcher fixes Copilot/gpt-5.6-sol-fast/xhigh, native `--ci-dir=/work/ci`,
initialization max-parallel 1, max-turns 0, new-run policy retries 2, transient
resumptions 3, and aggregate TLC limits 12G/four workers. Incremental mode uses
one conversation and does not receive the unsupported `--max-parallel` option.
Resume restores saved parallelism and both recovery budgets rather than
overriding them with new defaults.
Native TLC defaults are intentionally **not patched**: guidance/tool calls must
specify `-m 6G -M 2G -w 4`. Oversized defaults fail admission, never become a pass.
All current skills and required MCP helpers are installed. TLC/context tools are
registered by native per-phase orchestration. Native compaction is optional;
preflight proves dependencies, not a live provider compaction transaction.

Copilot retains `--disable-builtin-mcps --no-remote-export --no-ask-user
--no-auto-update --no-bash-env`, denies gh/push tools, and uses
`--secret-env-vars` to strip credentials from shell/MCP children and redact
output. Git/gh wrappers also block normal publication paths. These are practical
guardrails, **not an adversarial security boundary**: the model process has to
read its dedicated secret and network access remains available for model calls
and public dependencies. Never provide a personal/admin identity. Report-only
product behavior still requires guidance and source-diff review; legitimate
instrumentation/reproduction modifies private copies.

## Source and compiler preparation

Use ordinary complete clones, not linked worktrees or partial clones. Hydrating
every Git object is insufficient if `.promisor` markers remain: native source
isolation rejects them. The prepared `repos/ci-a`, `ci-b`, and `ci-current`
are complete nonpartial clones made with `git clone --no-local`.

The image pins `libprotoc 27.1` outside the source tree, exports `PROTOC` and
`PROTOC_INCLUDE`, and checks the executable hash. OpenVMM's native GNU/MSHV
build accepts this inherited override. Do not restore unrelated package sets
merely to discover protoc: package restoration creates absolute symlinks that
native source isolation rejects, including ignored build directories.

The retained package at `cache/protoc-27.1` came from the initial run's normal
download of `protoc-27.1-linux-x86_64.zip`. Its compiler SHA-256 is recorded in
`experiment.json` and the Dockerfile. It is required build input, not a silent
fallback to a system compiler. Preserve it with the runtime caches.

## Attempts and status

The default total timeout is six hours. `--timeout-seconds` changes it;
`--phase-timeout-seconds` bounds each native CLI invocation (also default six
hours). Each launch retains `/work/runtime/<attempt>/console.log`, command,
readiness, cgroup samples, container policy/state, and `status.json`. Timeout
stops only that container; progress, reports, and native sessions are retained.
Native temporary provider/transport failures can resume up to three times for
new conversations, with backoff of 4, 8, and 16 seconds and failed-attempt log
archives. Native policy recovery permits two revised continuations of the
legitimate verification task, without disabling provider filtering. The two
budgets are independent; exhaustion remains a failure. No automatic outer-job
restart or provider switch occurs. Retained old runs keep their saved budgets
and must use their original image/launcher/environment. Existing provider holds
are not cleared by this configuration change.

Native continuation needs an unfinished conversation. If a failure happens
between phases before the next conversation is created, `--run-id` cannot
resume it. Preserve the failed run and reuse completed assets with supported
BYOM initialization; do not invent a model/checkpoint or repeat completed
analysis. The experiment used this path once for the package-link failure.

For the controlled continuation exercise, SIGINT was sent only to the
reverified, owned Copilot process. Its wrapper and event-stream parser remained
alive, allowing Copilot's terminal result to save the exact session UUID.
Native handoff correctly rejected missing artifacts despite agent exit zero;
the same run/session subsequently completed the interrupted phase.
Do not infer an active Copilot session ID from timestamps, interrupt arbitrary
PIDs, or assume a hard container kill always flushes a resume checkpoint.

`native_complete_bug_fail` means exit 2 **and a new complete FAIL receipt**;
the valid native model may have published. Other native completion states are
`native_complete_pass` and `native_complete_warning`. `native_incomplete`,
`policy_stop`, `phase_timeout`, `wrapper_timeout`, `wrapper_oom`,
`wrapper_interrupted`, and `wrapper_error` are not success. Exit 0 without a new
native completion receipt is rejected. A separate acceptance/convergence gate
must not equate native completion or `current` with accepted exhaustive proof.

### Explicit imported starting models

The local `../import_baseline.py` command can adopt a frozen initialization model as an UNVERIFIED starting base in a separate store. This integration-level import validates retained asset hashes and source identity, creates an audit record, and uses native snapshot integrity checks. It does not create a completed verification receipt or change the original run. Inspection and later candidate promotion distinguish an explicitly imported base from a normally completed model; incomplete runtime output is never imported automatically. Execution holds remain unchanged.

Run controller tests without model calls:

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s .github/specula/controller/tests -v
```

`tests_native/test_runtime_contract.py` runs only inside this pinned runtime.
Mount an existing checkout with `--source` (the parser inspects its existence),
and mount a retained controller bundle with `--control`. The suite checks actual
native argument parsing, saved-budget restoration and mocked adapter outcomes;
it never invokes a provider, VM, or TLC.
