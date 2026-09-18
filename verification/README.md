# Verus verification

The initial verification target specifies `LoadedVm::restore_snapshot_state` through successful return with the guest still stopped.

```bash
make verify-setup
make verify
make verify MODULE=restore
make verify-smoke
```

`make verify` and `make verify MODULE=restore` are equivalent. Unknown module names fail. Verification builds the Verus fork and exact commit pinned by `verification/verus-repository` and `verification/verus-revision`, checks the resulting version against `verification/verus-version`, writes output under `target/verus/`, and never substitutes `cargo check` for Verus. The verifier, cargo-verus, builtin crates, and `vstd` therefore always come from the same source revision.

Current status:

- the original conditional `InitializedVm::load` contract is retained, and the focused contract is attached to `LoadedVm::restore_snapshot_state`;
- production-body proof is intentionally not part of the current task;
- snapshot preparation, destination construction, component refinement, caller failure, resume, and readiness are explicit separate obligations.

The restore documents are split by boundary:

- `restore/SPEC.md`: human-reviewable restore semantics and production theorem;
- `restore/PREPARATION.md`: snapshot artifact, memory, manifest, and worker-input preparation;
- `restore/COMPONENTS.md`: stable state-unit identities and component blob interpretation;
- `restore/UNINTERP.json`: remaining production View bridges;
- `restore/LIMITATION.json`: current Verus frontend limitations and workarounds.
