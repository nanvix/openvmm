# Snapshot Restore TOP-Level Specification

## Target boundary

The focused verification TOP is `LoadedVm::restore_snapshot_state` in `openvmm/openvmm_core/src/worker/dispatch.rs`.

Its pre-state is a destination `LoadedVm` after `InitializedVm::load` has completed platform/device construction, installed the prepared memory and external resources, and instantiated the selected VPs. Its successful post-state is the point after state-unit restore, permitted time adjustment, backend-clock adjustment, and acquisition of the restore stop guard, but before `LoadedVm::resume` can permit guest execution.

`InitializedVm::load` retains its conditional snapshot wrapper contract. That contract states the externally visible composition from the initialized VM and restore request to the returned `LoadedVm`, while delegating the extracted restore transition to the helper specification. It is not the focused verification target.

`prepare_snapshot_restore_for_config`, snapshot decoding, destination construction proofs, caller failure publication, deferred-state activation, and readiness publication are outside the focused TOP. They require separate contracts for an end-to-end proof, but this task does not add their proofs.

## Logical state boundary

The human-owned open model separates:

- `VmStateView`: the full logical runtime state represented at this boundary, including guest-visible state, destination configuration/resources, and `HostOperationalStateView`.
- `SnapshotVmStateView`: only state represented by snapshot artifacts and relevant to restore correctness: prepared RAM, partition and stable-identity VP state, component inventory and active/pending component state, and virtual time.
- `SavedVmStateView`: the component state carried by decoded `SavedState`; it does not contain RAM, external resources, destination compatibility, VP capacity, or snapshot-generation identity.
- `RestoreRequestView`: `SavedVmStateView`, selected VP count, and the optional downtime policy.
- `LoadedVmView`: the concrete helper pre/post-state together with active VP count and lifecycle phase.

`snapshot_state(vm_state)` is the explicit projection from full runtime state to snapshot state. It intentionally excludes compatibility metadata, destination VP capacity, external resource identities, and host-operational state such as tasks, sockets, signaling objects, wakers, and transient host queue occupancy.

Host-side activity may change `host_operational_state` while restore runs. The success condition therefore does not require equality for that field.

## Snapshot data interpretation

The generic serialized state is:

```text
SavedState {
    units: Vec<SavedStateUnit>,
    inventory: Vec<String>,
}

SavedStateUnit {
    name: String,
    state: SavedStateBlob,
}
```

`inventory` is the complete ordered state-unit identity set, including stateless units. `units` contains only units with mutable serialized state. Each `name` selects one registered component, and its opaque blob is interpreted by that component's concrete saved-state schema.

The TOP currently uses `decoded_restore_request_view` as an explicit proof-debt bridge from `SavedState` and `restore_time` to the logical request. Replacing that bridge with component Views is proof work and is not part of this top-level-spec task. Protobuf codec correctness may eventually be a narrow trusted boundary, but repository-owned name-to-component interpretation must not be hidden by the final proof.

## Preconditions

`RestoreRequestView::valid_for_loaded_vm(old(self)@)` requires:

- the helper pre-state is `PreparingRestore`;
- the active VP count does not exceed destination VP capacity;
- the request's selected VP count equals the active VP count already instantiated in the helper pre-state;
- the destination VP map has stable identities for the full destination capacity;
- saved VP identities are a subset of destination VP identities;
- the complete saved component inventory equals the destination component inventory;
- active and pending saved-state domains are subsets of that inventory and of the corresponding destination component domains;
- saved virtual time has not already incorporated restore downtime.

Preparation establishes additional end-to-end facts before this TOP: exact snapshot memory generation, manifest/state association, machine compatibility, approved external resources, decoded state provenance, and selected-VP construction. Those obligations are documented in `PREPARATION.md` and are not asserted by `SavedState` alone.

## Successful postcondition

On `Ok(())`, `snapshot_restore_success(old(self)@, request, final(self)@)` requires:

- the final snapshot-state projection equals `restore_snapshot_projection(snapshot_state(initial.state), request, initial.active_vp_count)`;
- prepared RAM remains the RAM already installed in the helper pre-state;
- saved partition state is restored;
- VP state is restored by stable VP identity for selected VPs present in the saved state, while other destination VP state remains initial/default;
- complete component inventory is preserved and saved active/pending component state overlays the initial/default component state;
- virtual time applies exactly the optional downtime adjustment defined by `restored_virtual_time`;
- destination compatibility, VP capacity, external resources, and active VP count are preserved;
- the final lifecycle phase is `PreExecutionRestored`.

The postcondition deliberately does not constrain `host_operational_state`. It also does not claim guest readiness, deferred-state activation, caller publication, or rollback after failure.

The `Err` arm has no state rollback guarantee. Failure non-publication and no-resume properties belong to separate caller contracts.

The shared `snapshot_restore_result` predicate contains the single definition of the successful restore result: snapshot projection equality, preserved destination frame, selected active VP count, and `PreExecutionRestored` lifecycle phase. `snapshot_restore_success` adapts the helper's `LoadedVm` pre-state to that predicate, while `snapshot_load_success` adapts the wrapper's `InitializedVm` pre-state. Neither wrapper duplicates the restore semantics.

The retained `InitializedVm::load` contract uses `valid_for_initialized_vm` and `snapshot_load_success`. It keeps the original conditional behavior for `saved_state.is_some()`, including boot-online and selected-VP bounds.

## Open and closed layers

The human-owned open layer in `worker/dispatch.spec.rs` defines the reviewable state vocabulary, projection, validity relation, and success property.

The closed layer in `worker/dispatch.proof.rs` currently provides only representation bridges needed to attach the open contract to production types. These bridges are recorded in `UNINTERP.json` as proof debt. No proof, `assume`, `admit`, copied restore implementation, or predicate that directly asserts the final theorem is added by this task.

`#[verus_spec]` contracts are attached to both the retained `InitializedVm::load` wrapper and `LoadedVm::restore_snapshot_state`. `verification/tools/verify.sh` selects the extracted helper as the focused TOP. The `#[verus_verify]` marker remains disabled because proving either body is outside the requested scope.
