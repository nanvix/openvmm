# Snapshot Restore Specification

## Target boundary

The sole TOP function is the successful `saved_state.is_some()` branch of `InitializedVm::load` in `openvmm/openvmm_core/src/worker/dispatch.rs`, ending when `load` returns and before `LoadedVm::resume` can release the restore stop guard. Snapshot file opening, manifest validation, saved-state decoding, memory-backing preparation, later guest repair, deferred device activation, and externally-ready publication are caller or later-phase protocols.

The specification therefore describes `PreExecutionRestored`, not a fully resumed VM. `LoadedVmView` records this as a lifecycle phase rather than independent `restored`, `running`, and `externally_ready` booleans. Failure does not promise rollback; failure-side non-publication is a caller obligation because publication state is not observable from `anyhow::Error` or from a failed `load` result.

## Inputs and shared logical state

The logical model follows the two-view design required by the human how-to specification. `SavedVmStateView` represents the component state actually carried by the decoded `SavedState`. `VmStateView` represents the abstract state of a concrete initialized or loaded VM. `restore_projection(initial, saved, policy)` constructs the complete `LoadedVmView` expected after restore. The projection, rather than an uninterpreted request field, determines which state comes from saved state and which state remains from the destination VM.

`decoded_restore_request_view` abstracts the already-decoded `SavedState` together with the caller-supplied restore time and VP-selection policy. It produces `SavedVmStateView` plus the policy; it must not invent memory, external-resource, compatibility, or snapshot-identity information that is absent from those arguments. The serialized snapshot-file-to-`SavedState` correspondence remains the explicitly trusted decoding boundary required by the human how-to specification.

The intended initial and loaded VM views describe the concrete input `InitializedVm` and returned `LoadedVm`; they are not replacement executable structs. They are declared in `worker/dispatch.spec.rs`, while their standard `View` implementations and closed representation mappings live in `worker/dispatch.proof.rs`. The remaining component correspondence functions are explicit `uninterp spec fn` proof debt recorded in `UNINTERP.json` and must be replaced by component Views as the frontend limitations are removed.

The caller must establish artifact validation, memory preparation, machine compatibility, and external-resource identity before invoking `load`. At the `load` boundary, memory, compatibility, and external resources are already part of `InitializedVmView` and are frame-preserved by `restore_projection`; `SavedState` is not treated as if it contained those values. Host virtual addresses, file descriptor numbers, worker identities, `StateUnits` registration mechanics, and task allocation details are intentionally hidden.

Because `InitializedVm::load` also implements the non-snapshot boot path, these requirements are conditional:

```text
saved_state.is_some() ==> decoded_restore_request_view(...).valid_for(self@)
```

The success guarantee is conditional for the same reason:

```text
saved_state.is_some() ==> snapshot_restore_success(self@, request, loaded@)
```

No snapshot-specific requirement or postcondition is imposed when `saved_state` is `None`.

## Successful restore guarantee

On success, before guest execution:

- RAM, compatibility, external resources, and destination VP capacity remain those of the already-prepared initial VM.
- VP state is keyed by stable VP index rather than represented only by sequence position. Saved state replaces selected VP identities that it contains; every other destination VP retains its initial/default state.
- The selected active VP count remains between the boot-online lower bound and destination VP capacity. The compatibility relation allows the snapshot VP sequence to be a prefix of the destination capacity, as required by the human how-to specification; whether every current backend implementation supports that case is a production proof obligation rather than a specification assumption.
- For capability-dependent optional CPU fields, `Some(value)` writes the saved value while `None` leaves the destination's initial field unchanged.
- `VirtualTimeView` advances by the requested downtime. Its VM-time component uses `floor(downtime_ns / 100)` 100ns ticks with `u64` wrapping. TSC, backend-clock, LAPIC, RTC, and timer implementation details refine this single abstract elapsed-time observation in component proofs rather than appearing as TOP success flags.
- The complete saved component inventory equals the destination component inventory, matching production `validate_inventory`. Mutable component-state maps are separate and may cover only a subset of that inventory. Saved active and pending state overlay the destination's initial/default component state.
- Disabled virtio queues do not require ring-address validation. Enabled queues retain configuration and progress and remain subject to the production ring-span, alignment, overlap, and guest-memory checks.
- The returned VM is in `PreExecutionRestored`: its restore stop guard remains held and guest execution has not begun. Externally-ready publication occurs later and is intentionally not represented as a field of `LoadedVmView`.

## Supported microVM state-unit profile

The stable core microVM ABI v1/v2 inventory includes `partition`, `vp0`, `vmtime`, `pic`, `ioapic`, `lapic`, `pit`, `rtc`, `microvm-portb`, `microvm-shutdown`, and `microvm-snapshot-request`. Chipset devices are registered as children of the `chipset` state unit using runtime names supplied by the builder. Optional virtio devices use configuration-derived stable names. VMBus and VTL2 VMBus are conditional and are not guaranteed by the core microVM profile.

Detailed ownership, restore mode, source references, and remaining external contracts are recorded in `COMPONENTS.md`.

## Current proof boundary

The old standalone executable model has been removed. The current `worker/dispatch.spec.rs` contains only specification-mode Views and relations over the real production `InitializedVm`, `SavedState`, and `LoadedVm` types. It is included from `worker/dispatch.rs` and does not contain a second restore implementation.

The Human-owned open layer consists of:

- `restore_projection`, which maps decoded saved state, destination VM state, and restore policy to the complete expected `LoadedVmView`;
- `restore_vp_projection`, which restores saved VP state by stable VP index and preserves initial/default state for all other VP identities;
- `snapshot_restore_success`, which requires the real returned `LoadedVmView` to equal that complete projection;
- `RestoreRequestView::valid_for`, which groups the conditional VP-selection, inventory, component-domain, and saved-time requirements that `load` can consume from its actual inputs;
- `saved_state_is_compatible_with`, which distinguishes exact component inventory compatibility from the subset of components carrying mutable state;
- `boot_vp_count_is_valid`, which exposes the caller-visible VP selection constraint.

The Engineer-owned closed layer is in `worker/dispatch.proof.rs`, included from `worker/dispatch.rs` as the `restore_proof` module. Its current `pre_execution_representation` predicate records how the production `LoadedVm` fields represent the pre-execution state. Future internal invariants and proof lemmas belong there rather than in the Human-owned open specification.

`openvmm_core` opts into cargo-verus, depends on the pinned repository-local Verus source, and places an exhaustive `#[verus_spec]` result match directly on the production `InitializedVm::load` declaration. The separate `#[verus_verify]` marker is intentionally commented out rather than duplicated with `verus_spec`. The success branch uses the standard `self@` and `loaded@` Views and additionally records the closed production representation fact.

`InitializedVm::load` cannot express the caller-visible failure guarantee from an `anyhow::Error`: the error does not contain guest-execution or publication state. Its error arm therefore makes no extra state claim. The no-resume/no-publication property belongs to the caller orchestration that owns those actions and must be specified there.

The caller proof must cover both production call sites:

- `VmWorker::new`: if `load` returns `Err`, control returns through `?` before assigning `restore_ready_sink`, calling `LOADED_VM.store`, constructing `VmWorker`, entering `run`, or publishing restore readiness.
- `VmWorker::restart`: if `load` returns `Err`, control returns through `?` before `LOADED_VM.store` and before the conditional `resume`.

These obligations implement the lifecycle transition `Preparing -> FailedBeforePublication`. They must eventually be attached to the real caller bodies using a caller-owned event/publication View; they must not be reconstructed from the error payload.

The production body is not yet verified. Cargo-verus reaches the real async body and has advanced past the earlier tracing `__CALLSITE`, function-local constant/type, `anyhow::ensure!` formatting, trait-object lowering, byte-string constant, array-pattern, and generator limitations. The remaining immediate blocker is in the pinned Verus toolchain rather than an unsupported source expression: pruning an async root unconditionally marks the private `vstd::future::exec_await` hook reachable, but the Cargo-built vstd metadata does not export a corresponding function entry, causing `vir/src/prune.rs` to panic with `no entry found for key`. A diagnostic verifier confirmed the missing key exactly as `Fun(Path(vstd, ["future" :: "exec_await"]))`.

With a diagnostic-only prune guard, translation proceeds to the next legitimate BOTTOM modeling frontier: the production `HvlitePartition` trait and its `Inspect`/`RequestYield` supertrait closure are not declared to Verus. This interface must be modeled explicitly, with narrow contracts for the restore-relevant clock and partition operations; it must not be bypassed with an empty external trait, broad `external_body`, copied executable, assumption of the TOP restore relation, or a `verus_keep_ghost`-selected replacement.
