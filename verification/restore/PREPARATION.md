# Snapshot Restore Preparation Boundary

## Production pipeline

The restore preparation path precedes `InitializedVm::load`:

```text
OpenedSnapshot::open
    -> prepare_snapshot_restore_for_config
    -> PreparedSnapshotRestore
    -> VmWorkerParameters
    -> VmWorker::new
    -> InitializedVm::new
    -> InitializedVm::load
    -> LoadedVm::restore_snapshot_state
```

`prepare_snapshot_restore_for_config` is an upstream proof unit, not an implementation detail that may be hidden behind the `load` theorem.

## Concrete prepared values

`PreparedSnapshotRestore` contains:

- `shared_memory`: a copy-on-write mapping handle duplicated from the exact opened `memory.bin`;
- `guards`: the opened directory, manifest, state, and memory handles that pin one snapshot generation;
- `saved_state`: the decoded protobuf message from `state.bin`;
- `restore_time`: validated downtime, TSC frequency, optional APIC frequency, and CPU compatibility contract.

The runtime `SavedState` parsed later by `VmWorker::new` contains only:

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

`SavedStateBlob` is an opaque protobuf payload at the generic state-unit layer. Its meaning is determined by the stable unit name and the concrete component's `SavedState` schema.

## Preparation theorem

On successful return, preparation must establish:

1. The manifest, `state.bin`, and `memory.bin` belong to one opened and pinned snapshot generation.
2. Architecture, page size, memory size, VP capacity, ABI version, and the supported machine contract are compatible with the destination configuration.
3. Manifest state-unit names equal `SavedState.inventory`.
4. The prepared memory mapping refers to the validated memory generation and covers the declared guest RAM layout.
5. The calculated downtime and saved TSC/APIC frequencies satisfy their bounds and originate from the validated machine contract.
6. CPU, filesystem, network, console, block, and other attachment identities match the caller-approved destination resources.
7. The single-use restore claim succeeds before worker construction.

These facts establish the pre-state consumed by `LoadedVm::restore_snapshot_state`: snapshot RAM and approved external resources have already been installed, destination compatibility and VP capacity have been validated, the selected VPs have been instantiated, and `SavedState` plus the restore-time policy correspond to the same opened snapshot generation.

This preparation theorem is required for end-to-end verification, but it is outside the current TOP-level specification task. The helper contract must not reconstruct snapshot provenance from `SavedState` alone, and it must not claim that preparation has been proved.

## Verification and trusted boundaries

Repository-owned validation remains in the proof scope:

- manifest and machine-contract validation;
- inventory equality;
- size, offset, range, and generation checks;
- downtime bounds;
- resource-identity matching;
- construction of the prepared worker parameters.

The following may use narrow trusted contracts:

- protobuf codec correctness: successful decode corresponds to the supplied bytes;
- OS opened-file identity and metadata;
- copy-on-write mapping primitives after repository-owned validation;
- wall-clock sampling as an explicit environment input.

No preparation contract may assert the final `snapshot_restore_success` result.
