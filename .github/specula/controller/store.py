#!/usr/bin/env python3
"""Image-local native store operations, without an agent or GitHub client."""

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import stat
import sys

from specula import ci_init
from specula.ci_store import CIError, CIStore, asset_hashes, freeze_source, git, read_json, write_json
from specula.snapshotlib import SourceSnapshot, SnapshotError, _validate_source_tree


IMPORT_KIND = "imported_unverified"
IMPORT_VERIFICATION = (
    "Explicitly imported UNVERIFIED initialization artifacts. No completed verification "
    "workflow or finalized finding classification is asserted; retained evidence requires verification."
)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def relative_path(value):
    if (not isinstance(value, str) or not value or Path(value).is_absolute()
            or ".." in Path(value).parts or Path(value).as_posix() != value or value == "."):
        raise CIError(f"unsafe import path: {value!r}")
    return value


def run_identity(value):
    if not isinstance(value, str) or re.fullmatch(r"[A-Za-z0-9_-][A-Za-z0-9._-]*", value) is None:
        raise CIError("invalid import run identity")
    return value


def regular_path(root, relative):
    relative_path(relative)
    if not ci_init._regular_file(root, relative):
        raise CIError(f"import requires a regular, non-symlink file: {relative}")
    return root / relative


def manifest_inputs(manifest, inputs, revision, target, baseline):
    parts = Path(relative_path(baseline)).parts
    if (len(parts) != 4 or parts[:2] != ("ci-init", "baselines")
            or parts[3] != "ci-baseline.json"):
        raise CIError("import requires an explicitly selected frozen initialization manifest")
    if not isinstance(revision, str) or re.fullmatch(r"[0-9a-f]{40}", revision) is None:
        raise CIError("import requires the full source commit")
    if (not isinstance(target, str) or not target or inputs.get("target") != target
            or manifest.get("target") != target.split("|", 1)[0]):
        raise CIError("import target differs from the original native inputs")
    old_id = run_identity(manifest.get("run_id"))
    if (manifest.get("version") != 1 or manifest.get("kind") != "ci-initialization"
            or manifest.get("validation_status") != "UNVERIFIED"
            or type(manifest.get("pipeline_exit_code")) is not int
            or manifest["pipeline_exit_code"] < 0 or manifest.get("missing_artifacts") != []
            or inputs.get("version") != 1 or inputs.get("dirty") is not False
            or inputs.get("previous") is not None or inputs.get("source_commit") != revision
            or inputs.get("source") != f"runs/{old_id}/ci-source"
            or not isinstance(inputs.get("guidance"), str)
            or not isinstance(manifest.get("run_root"), str)
            or not Path(manifest["run_root"]).is_absolute()
            or Path(manifest["run_root"]).name != old_id):
        raise CIError("import manifest and clean initialization source identities differ")
    snapshot = inputs.get("snapshot_commit")
    if not isinstance(snapshot, str) or re.fullmatch(r"[0-9a-f]{40}", snapshot) is None:
        raise CIError("import requires the full saved snapshot commit")
    source_record = manifest.get("source")
    if not isinstance(source_record, dict):
        raise CIError("import manifest must contain original source identities")
    for phase in ("before", "after"):
        source = source_record.get(phase)
        if (not isinstance(source, dict)
                or source.get("commit") != revision or source.get("original_commit") != revision
                or source.get("dirty") is not False or source.get("source_mode") != "snapshot"):
            raise CIError("manifest source does not match the requested clean source pin")
    expected_assets = (Path(baseline).parent / ".specula-output").as_posix()
    if manifest.get("assets") != expected_assets:
        raise CIError("manifest assets must be the selected frozen sibling package")
    files = manifest.get("files_sha256")
    if not isinstance(files, dict) or not files:
        raise CIError("import manifest has no asset hashes")
    for name, value in files.items():
        relative_path(name)
        if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None:
            raise CIError("invalid import asset hash")
    if not {"spec/base.tla", "harness/run.sh"} <= files.keys():
        raise CIError("import requires the reference model and harness")
    # This contract deliberately has no final classification to seed or report.
    if "ci-verdict.json" in files or "confirmed-bugs.md" in files or "spec/issues/index.json" in files:
        raise CIError("unverified import must not contain a finalized finding classification")
    omitted = manifest.get("omitted_paths")
    if not isinstance(omitted, list):
        raise CIError("import manifest must record omitted paths")
    for name in omitted:
        relative_path(name)
        if name in files:
            raise CIError("manifest lists an asset as both copied and omitted")


def validate_assets(assets, files):
    if asset_hashes(assets) != files:
        raise CIError("frozen import assets differ from the manifest hashes")
    for name in ("spec/base.tla", "harness/run.sh"):
        if not regular_path(assets, name).read_bytes().strip():
            raise CIError(f"empty required import asset: {name}")


def validate_source(source, revision, snapshot):
    metadata = source / ".git"
    if not stat.S_ISDIR(metadata.lstat().st_mode):
        raise CIError("import source must be an ordinary independent Git clone")
    if any(path.is_symlink() for path in metadata.rglob("*")):
        raise CIError("import Git metadata must not contain external links")
    for name in ("objects/info/alternates", "objects/info/http-alternates", "commondir", "shallow", "info/grafts"):
        path = metadata / name
        if path.exists() or path.is_symlink():
            raise CIError("import source must not depend on external or incomplete Git history")
    if git(source, "rev-parse", "--git-common-dir") != ".git" or git(source, "replace", "--list"):
        raise CIError("import source has redirected Git history")
    _validate_source_tree(source)
    if (git(source, "rev-parse", "HEAD") != snapshot
            or git(source, "--no-optional-locks", "status", "--porcelain", "--untracked-files=all")):
        raise CIError("saved import source is dirty or at the wrong snapshot")
    tree = git(source, "rev-parse", f"{revision}^{{tree}}")
    if git(source, "rev-parse", f"{snapshot}^{{tree}}") != tree:
        raise CIError("clean source pin and saved snapshot have different trees")
    return tree


def import_state(store, run_id, origin, manifest, inputs, tree):
    source = f"runs/{run_id}/ci-source"
    identity = hashlib.sha256(json.dumps(origin, sort_keys=True).encode()).hexdigest()
    return {
        "version": 1, "target": inputs["target"], "artifact": str(store.path(source)),
        "source": source, "source_commit": inputs["source_commit"],
        "snapshot_commit": inputs["snapshot_commit"], "dirty": False,
        "guidance": inputs["guidance"], "run_id": run_id, "files_sha256": manifest["files_sha256"],
        "verification": IMPORT_VERIFICATION, "verdict": "UNVERIFIED", "previous": None,
        "check_key": f"imported-unverified:{identity}", "source_tree": tree,
        "checked_source_commit": None, "evidence_run_id": origin["source_run_id"],
        "baseline_kind": IMPORT_KIND, "verification_complete": False,
        "findings_finalized": False, "origin": origin,
        "source_control_sha256": {
            name: digest(regular_path(store.path(source), f".git/{name}"))
            for name in ("config", "info/attributes")
        },
    }


def import_audit(token, state_hash, origin):
    return {
        "version": 1, "kind": IMPORT_KIND, "run_id": Path(token).parts[1],
        "snapshot": token, "verdict": "UNVERIFIED", "verification_complete": False,
        "findings_finalized": False, "state_sha256": state_hash, "origin": origin,
    }


def imported_snapshot(store, token):
    state = store.snapshot(token)
    run_id = run_identity(state["run_id"])
    if token != f"runs/{run_id}/ci-published/{run_id}":
        raise CIError("import publication token and run identity differ")
    directory = store.path(token)
    run = store.path(f"runs/{run_id}")
    result = run / "ci-result.json"
    if result.exists() or result.is_symlink():
        raise CIError("an unverified import must not have a native completion receipt")
    audit = read_json(regular_path(run, "ci-import.json"))
    origin = audit["origin"]
    manifest_path = regular_path(directory, "origin/ci-baseline.json")
    inputs_path = regular_path(directory, "origin/ci-input.json")
    if digest(manifest_path) != origin["manifest_sha256"] or digest(inputs_path) != origin["input_sha256"]:
        raise CIError("import archived provenance was modified")
    manifest, inputs = read_json(manifest_path), read_json(inputs_path)
    manifest_inputs(manifest, inputs, origin["source_commit"], origin["target"], origin["baseline"])
    expected_origin = {
        "seed_run": origin["seed_run"], "source_run_id": manifest["run_id"],
        "source_run_root": manifest["run_root"], "baseline": origin["baseline"],
        "manifest_sha256": digest(manifest_path), "input_sha256": digest(inputs_path),
        "source_commit": inputs["source_commit"], "snapshot_commit": inputs["snapshot_commit"],
        "target": inputs["target"], "validation_status": manifest["validation_status"],
        "pipeline_exit_code": manifest["pipeline_exit_code"],
    }
    if (origin != expected_origin or not isinstance(origin["seed_run"], str)
            or not Path(origin["seed_run"]).is_absolute() or run_id == manifest["run_id"]):
        raise CIError("import audit provenance is inconsistent")
    source = store.path(f"runs/{run_id}/ci-source")
    tree = validate_source(source, inputs["source_commit"], inputs["snapshot_commit"])
    expected = import_state(store, run_id, origin, manifest, inputs, tree)
    if state != {**expected, "token": token, "model_path": str(directory / "model")}:
        raise CIError("import state differs from its audited unverified contract")
    if audit != import_audit(token, digest(directory / "state.json"), origin):
        raise CIError("import audit differs from its snapshot")
    validate_assets(directory / "model", manifest["files_sha256"])
    return state, audit


def import_baseline(root, *, seed_run, baseline, manifest_sha256, revision, target, run_id):
    run_identity(run_id)
    if (not isinstance(manifest_sha256, str)
            or re.fullmatch(r"[0-9a-f]{64}", manifest_sha256) is None):
        raise CIError("import requires an explicit SHA-256 manifest pin")
    seed = Path(seed_run).absolute()
    destination_root = Path(root).resolve()
    if destination_root.is_relative_to(seed.resolve()) or seed.resolve().is_relative_to(destination_root):
        raise CIError("import store and read-only evidence must be separate trees")
    store = CIStore(Path(root))
    store.acquire()
    try:
        token = f"runs/{run_id}/ci-published/{run_id}"
        current = store.current_token()
        if current is not None and current != token:
            raise CIError("import refuses to replace an unrelated current")
        if not stat.S_ISDIR(seed.lstat().st_mode):
            raise CIError("seed run must be a real read-only source directory")
        manifest_path = regular_path(seed, baseline)
        inputs_path = regular_path(seed, "ci-input.json")
        if digest(manifest_path) != manifest_sha256:
            raise CIError("import manifest SHA-256 differs from the requested pin")
        manifest, inputs = read_json(manifest_path), read_json(inputs_path)
        manifest_inputs(manifest, inputs, revision, target, baseline)
        if run_id == manifest["run_id"]:
            raise CIError("an import must use a new run identity")
        assets = CIStore(seed).path(manifest["assets"])
        validate_assets(assets, manifest["files_sha256"])
        seed_source = CIStore(seed).path("ci-source")
        tree = validate_source(seed_source, revision, inputs["snapshot_commit"])
        origin = {
            "seed_run": str(seed), "source_run_id": manifest["run_id"],
            "source_run_root": manifest["run_root"], "baseline": baseline,
            "manifest_sha256": manifest_sha256, "input_sha256": digest(inputs_path),
            "source_commit": revision, "snapshot_commit": inputs["snapshot_commit"],
            "target": target, "validation_status": manifest["validation_status"],
            "pipeline_exit_code": manifest["pipeline_exit_code"],
        }
        if current == token:
            state, audit = imported_snapshot(store, token)
            if audit["origin"] != origin:
                raise CIError("repeated import differs from the original audited provenance")
            return {"current": describe(state)}
        if set(path.name for path in store.root.iterdir()) != {".lock", "runs"} or any(
                (store.root / "runs").iterdir()):
            raise CIError("import requires an empty new private store; partial imports are not resumed")
        run = ci_init._directory(store.root, f"runs/{run_id}")
        destination = ci_init._directory(run, f"ci-published/{run_id}")
        archive = ci_init._directory(destination, "origin")
        ci_init._write_once(archive / "ci-baseline.json", manifest_path.read_bytes())
        ci_init._write_once(archive / "ci-input.json", inputs_path.read_bytes())
        model = ci_init._directory(destination, "model")
        copied, omitted = ci_init._copy_assets(assets, model)
        if copied != manifest["files_sha256"] or omitted:
            raise CIError("native asset copy did not preserve the complete frozen package")
        # The native helper clones full history without local object sharing, then
        # fetches and checks out the exact pre-instrumentation snapshot.
        freeze_source(SourceSnapshot(
            original=seed_source, source=seed_source, baseline_git=seed_source,
            baseline=inputs["snapshot_commit"], patch=run / "unused-source.diff",
            is_git=True, reviewable_diff=True,
        ), run / "ci-source")
        # Native source snapshots can have no refs, leaving the product revision
        # unreachable from their orphan HEAD and therefore absent from the clone.
        git(run / "ci-source", "fetch", "--no-tags", str(seed_source),
            f"{revision}:refs/specula/import-source")
        state = import_state(store, run_id, origin, manifest, inputs, tree)
        write_json(destination / "state.json", state)
        write_json(run / "ci-import.json", import_audit(token, digest(destination / "state.json"), origin))
        state, _ = imported_snapshot(store, token)
        store.advance(token)
        return {"current": describe(state)}
    finally:
        store.close()


def completed_snapshot(store, token):
    state = store.snapshot(token)
    parts = Path(token).parts
    if (len(parts) != 4 or parts[0] != "runs" or parts[2] != "ci-published"
            or state["run_id"] != parts[1]):
        raise CIError("publication token and native run identity differ")
    if (state.get("baseline_kind") == IMPORT_KIND
            or store.path(f"runs/{state['run_id']}/ci-import.json").exists()):
        raise CIError("an audited unverified import is not a completed publication")
    receipt = read_json(store.path(f"runs/{state['run_id']}/ci-result.json"))
    if (receipt.get("complete") is not True or receipt.get("snapshot") != token
            or receipt.get("run_id") != state["run_id"]
            or receipt.get("verdict") != state["verdict"]
            or state.get("verdict") not in {"PASS", "WARNING", "FAIL"}
            or state.get("verification") != "Completed verification workflow."
            or state.get("dirty") is not False):
        raise CIError("publication lacks a consistent complete clean-source receipt")
    return state, receipt


def accepted_snapshot(store, token):
    state = store.snapshot(token)
    if (state.get("baseline_kind") == IMPORT_KIND
            or store.path(f"runs/{state['run_id']}/ci-import.json").exists()):
        return imported_snapshot(store, token)
    return completed_snapshot(store, token)


def describe(state):
    identity = {key: state[key] for key in (
        "token", "run_id", "target", "source_commit", "snapshot_commit",
        "previous", "verdict", "check_key",
    )}
    if state.get("baseline_kind") == IMPORT_KIND:
        return identity | {
            "baseline_kind": IMPORT_KIND, "verification_complete": False,
            "finding_counts": {}, "findings_finalized": False,
            "verification": state["verification"], "origin": state["origin"],
        }
    verdict = read_json(Path(state["model_path"]) / "ci-verdict.json")
    findings = verdict["findings"]
    return identity | {"finding_counts": dict(Counter(item["status"] for item in findings))}


def operate(root, operation, *, token=None, revision=None, target=None, verdict=None):
    store = CIStore(Path(root))
    store.acquire()
    try:
        current = store.current_token()
        if operation == "inspect":
            selected = token or current
            return {"current": describe(accepted_snapshot(store, selected)[0]) if selected else None}
        if not token or not revision or not target or verdict not in {"PASS", "WARNING", "FAIL"}:
            raise CIError("promotion requires the exact token, source, target and verdict")
        state, receipt = completed_snapshot(store, token)
        if state["source_commit"] != revision or state["target"] != target or state["verdict"] != verdict:
            raise CIError("publication does not match the outer runtime result")
        if operation == "verify-initialization":
            if receipt.get("candidate") is not False or state["previous"] is not None or current != token:
                raise CIError("initialization must be complete in its private, otherwise empty store")
        elif operation == "promote":
            if receipt.get("candidate") is not True or state["previous"] is None:
                raise CIError("only an incremental native candidate may be promoted")
            if current != token:
                if current != state["previous"]:
                    raise CIError("current advanced since candidate preparation")
                previous, _ = accepted_snapshot(store, current)
                git(store.path(state["source"]), "merge-base", "--is-ancestor",
                    previous["source_commit"], state["source_commit"])
                store.advance(token)
        else:
            raise CIError("unsupported native store operation")
        return {"current": describe(state)}
    finally:
        store.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("inspect", "promote", "verify-initialization", "import-baseline"))
    parser.add_argument("--ci-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--token")
    parser.add_argument("--revision")
    parser.add_argument("--target")
    parser.add_argument("--verdict")
    parser.add_argument("--seed-run", type=Path)
    parser.add_argument("--baseline")
    parser.add_argument("--manifest-sha256")
    parser.add_argument("--run-id")
    args = parser.parse_args()
    try:
        if args.operation == "import-baseline":
            if not all((args.seed_run, args.baseline, args.manifest_sha256,
                        args.revision, args.target, args.run_id)):
                raise CIError("import requires seed run, frozen baseline, manifest hash, source, target and new run ID")
            if args.output.resolve().is_relative_to(args.seed_run.resolve()):
                raise CIError("import output must not modify the read-only seed")
            args.output.parent.mkdir(parents=True, exist_ok=True)
            result = import_baseline(
                args.ci_dir, seed_run=args.seed_run, baseline=args.baseline,
                manifest_sha256=args.manifest_sha256, revision=args.revision,
                target=args.target, run_id=args.run_id,
            )
        else:
            result = operate(args.ci_dir, args.operation, token=args.token, revision=args.revision,
                             target=args.target, verdict=args.verdict)
        write_json(args.output, result)
        return 0
    except (CIError, ci_init.CIInitError, SnapshotError, OSError, ValueError, KeyError, TypeError) as exc:
        print(f"Native store operation refused: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
