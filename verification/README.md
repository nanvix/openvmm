# Verus verification

The initial verification target models snapshot restore through the successful return of `InitializedVm::load`, before guest execution.

```bash
make verify-setup
make verify
make verify MODULE=restore
make verify-smoke
```

`make verify` and `make verify MODULE=restore` are equivalent. Unknown module names fail. Verification uses the exact version in `verification/verus-version`, writes output under `target/verus/`, and never substitutes `cargo check` for Verus.

Current status:

- specification semantics and representative model checks: implemented;
- async `InitializedVm::load` production-body proof: not yet implemented;
- snapshot codec, memory preparation, state-unit, hypervisor, device, and execution-gate connections: explicit proof obligations in `restore/COMPONENTS.md`.
