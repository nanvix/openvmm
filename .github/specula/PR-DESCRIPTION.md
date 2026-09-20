# ci: add Specula bug-finding CI

## Summary

Add Specula-based bug finding for microVM snapshot/restore through incremental modeling and verification. Run on published releases or manual dispatch, not pushes or pull requests.

- Share the local and Actions driver. Reuse retained models to prepare the first compatible baseline, then run native incremental verification for the requested release.
- Use a pinned Docker runtime limited to 26 GiB RAM, no additional swap and 6 CPUs, with bounded recovery.
- Publish a job summary and result metadata; retain detailed findings, models and evidence under `/mnt/data`.
- Report only: no automatic product fixes, issues or PRs. Include a prompt template for separately approved Copilot fix work.

## Deployment

Requires a dedicated MSHV runner, provisioned runtime/guest fixtures, local credentials and execution authorization. Merge enables the release trigger; it does not provision these prerequisites.

See `.github/specula/README.md` for setup and operation.
