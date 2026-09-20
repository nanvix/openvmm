#!/usr/bin/env python3
"""Explicitly adopt retained initialization assets without claiming verification."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import uuid

import release


def adopt(config, *, seed_run, baseline, manifest_sha256, request_id,
          backend_factory=release.Backend, bundle_directory=release.HERE):
    if not release.ID.fullmatch(request_id):
        raise release.ReleaseError("invalid_request", "Invalid import request ID.")
    if not re.fullmatch(r"[0-9a-f]{64}", manifest_sha256):
        raise release.ReleaseError("invalid_request", "Supply the frozen manifest's SHA-256.")
    root, work = Path(config["root"]), Path(config["work"])
    release.confined(work, root)
    seed_run = release.confined(Path(seed_run), root)
    relative = Path(baseline)
    if relative.is_absolute() or ".." in relative.parts:
        raise release.ReleaseError("invalid_request", "Baseline must be a relative path inside the retained run.")
    manifest = release.confined(seed_run / relative, seed_run)
    if release.digest(manifest) != manifest_sha256:
        raise release.ReleaseError("invalid_seed", "Frozen initialization manifest hash differs.")
    inputs = release.read_json(release.confined(seed_run / "ci-input.json", seed_run))
    revision = inputs["source_commit"]
    if inputs["target"] != config["target"]:
        raise release.ReleaseError("invalid_seed", "Retained initialization belongs to another target.")
    release.Request("preflight", None, revision, None, request_id).validate()
    native_work = release.native_directory(work, work / "initializations" / request_id)
    if seed_run.is_relative_to(native_work) or native_work.is_relative_to(seed_run):
        raise release.ReleaseError("invalid_seed", "Import destination must be separate from original evidence.")
    directory = release.confined(work / "releases/requests" / request_id, work)
    directory.mkdir(parents=True, exist_ok=True)
    record = {
        "version": 1, "mode": "import-baseline", "request_id": request_id,
        "revision": revision, "target": config["target"], "image": config["image"],
        "environment_id": config["environment_id"], "native_work": str(native_work),
        "source_run": seed_run.name, "seed_run": str(seed_run),
        "baseline_manifest": relative.as_posix(), "manifest_sha256": manifest_sha256,
        "baseline_kind": "imported_unverified", "complete": False,
        "verification_complete": False, "remote_mutations": False,
        "started_at": release.runtime.now(), "status": "preparing_import",
    }
    identity = (
        "mode", "request_id", "revision", "target", "image", "environment_id",
        "native_work", "seed_run", "baseline_manifest", "manifest_sha256",
    )
    try:
        with release.lock(root / ".release.lock"), release.lock(root.parent / ".host.lock") as host_lock:
            operation = directory / "operation.json"
            if operation.exists():
                previous = release.read_json(operation)
                if any(previous.get(key) != record[key] for key in identity):
                    raise release.ReleaseError("request_identity_changed", "Import request identity changed.")
            config = {**config, "_host_lock": host_lock}
            release.reconcile_pending(config)
            bundle, bundle_id = release.stage_bundle(config, bundle_directory)
            record["control_bundle"] = bundle_id
            selected = release.active_work(work)
            active_config = {**config, "_native_work": str(selected)}
            current = backend_factory(active_config, bundle).store("inspect").get("current")
            if current is not None and (
                selected != native_work or current.get("run_id") != request_id
                or current.get("baseline_kind") != "imported_unverified"
            ):
                raise release.ReleaseError("already_initialized", "An existing baseline must not be replaced by an import.")
            release.runtime.write_json(operation, record)
            native_work.mkdir(parents=True, exist_ok=True)
            output = release.confined(native_work / "releases/internal/import.json", native_work)
            output.parent.mkdir(parents=True, exist_ok=True)
            backend = backend_factory({**config, "_native_work": str(native_work)}, bundle)
            args = [
                "python3", "/control/store.py", "import-baseline",
                "--ci-dir", "/work/ci", "--output", "/work/releases/internal/import.json",
                "--seed-run", "/seed", "--baseline", relative.as_posix(),
                "--manifest-sha256", manifest_sha256, "--revision", revision,
                "--target", config["target"], "--run-id", request_id,
            ]
            code, attempt, receipt = backend.invoke("exec", args, seed=seed_run)
            record["runtime_receipt"] = receipt
            if code or attempt["status"] != "command_success":
                raise release.ReleaseError("import_failed", "Baseline import failed; retained evidence and active selection are unchanged.")
            imported = release.read_json(output)["current"]
            if (
                imported.get("baseline_kind") != "imported_unverified"
                or imported.get("verification_complete") is not False
                or imported.get("verdict") != "UNVERIFIED"
                or imported.get("run_id") != request_id
                or imported.get("source_commit") != revision
                or imported.get("target") != config["target"]
            ):
                raise release.ReleaseError("invalid_import", "Import output does not match the requested unverified base.")
            if release.active_work(work) != selected:
                raise release.ReleaseError("current_changed", "Active work changed during import.")
            selector = release.confined(work / "releases/active-work.json", work)
            release.runtime.write_json(selector, {
                "version": 1, "work": str(native_work), "snapshot": imported["token"],
                "baseline_kind": "imported_unverified", "manifest_sha256": manifest_sha256,
                "selected_at": release.runtime.now(),
            })
            record.update(
                status="baseline_imported_unverified", exit_code=0,
                snapshot=imported["token"], native_run=request_id, verdict="UNVERIFIED",
            )
            return release.finish_report(directory, record)
    except release.ReleaseError as exc:
        record.update(status=exc.status, error=str(exc), exit_code=1)
    except (OSError, KeyError, TypeError, ValueError, subprocess.SubprocessError) as exc:
        record.update(status="import_error", error=str(exc), exit_code=1)
    directory = release.confined(work / "releases/rejections" / (request_id + "-" + uuid.uuid4().hex), work)
    directory.mkdir(parents=True)
    return release.finish_report(directory, record)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=release.HERE / "config.json")
    parser.add_argument("--seed-run", type=Path, required=True)
    parser.add_argument("--baseline", required=True)
    parser.add_argument("--manifest-sha256", required=True)
    parser.add_argument("--request-id", required=True)
    parser.add_argument("--acknowledge-unverified", action="store_true", required=True)
    args = parser.parse_args(argv)
    try:
        code, public, record = adopt(
            release.load_config(args.config), seed_run=args.seed_run,
            baseline=args.baseline, manifest_sha256=args.manifest_sha256,
            request_id=args.request_id,
        )
        print(json.dumps({"status": record["status"], "report": str(public), "exit_code": code}))
        return code
    except (release.ReleaseError, OSError, KeyError, TypeError, ValueError) as exc:
        print(f"Baseline import rejected: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
