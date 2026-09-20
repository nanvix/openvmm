"""SYNTHETIC import/preparation fixtures only: no model, provider, TLC or VM run."""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
sys.path.insert(0, str(Path(__file__).resolve().parent))
from store import IMPORT_KIND, import_baseline, operate
import test_store
from specula.ci_inheritance import inherit, matches_source
from specula.ci_store import CIError, CIStore, asset_hashes, git, read_json, write_json
from specula.ci_workflow import CIPipeline


class NativeImportTests(unittest.TestCase):
    publish = test_store.NativeStoreTests.publish

    def setUp(self):
        environment = patch.dict(os.environ, dict(os.environ))
        environment.start()
        self.addCleanup(environment.stop)
        self.temporary = tempfile.TemporaryDirectory(
            prefix="SYNTHETIC-native-import-", dir=os.environ["TMPDIR"],
        )
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.root = self.directory / "private-ci"
        self.seed = self.directory / "SYNTHETIC-interrupted-run"
        self.source = self.seed / "ci-source"
        self.source.mkdir(parents=True)
        git(self.source, "init", "--quiet", "--initial-branch=main")
        (self.source / "fixture.txt").write_text("SYNTHETIC source, not product verification\n")
        git(self.source, "add", "fixture.txt")
        git(self.source, "commit", "--quiet", "-m", "Synthetic import source fixture")
        self.revision = git(self.source, "rev-parse", "HEAD")
        tree = git(self.source, "rev-parse", "HEAD^{tree}")
        self.snapshot = git(self.source, "commit-tree", tree, "-m", "Synthetic native-style orphan snapshot")
        git(self.source, "checkout", "--quiet", "--detach", self.snapshot)
        git(self.source, "branch", "-D", "main")
        self.assertEqual(git(self.source, "for-each-ref"), "")
        self.target = "SYNTHETIC|fixture/project|Text|Unverified import contract only"
        self.baseline = "ci-init/baselines/synthetic-frozen-package/ci-baseline.json"
        self.manifest_path = self.seed / self.baseline
        self.assets = self.manifest_path.parent / ".specula-output"
        for name, text in {
            "spec/base.tla": "---- MODULE base ----\nVARIABLE x\n====\n",
            "spec/MC.tla": "SYNTHETIC MC fixture\n",
            "spec/MC.cfg": "SYNTHETIC MC config fixture\n",
            "spec/Trace.tla": "SYNTHETIC Trace fixture\n",
            "spec/Trace.cfg": "SYNTHETIC Trace config fixture\n",
            "spec/confirmation/MC-1/investigation.md": (
                "SYNTHETIC unfinished MC-1 investigation; NO final classification.\n"
            ),
            "harness/run.sh": "# SYNTHETIC fixture, never execute as verification\n",
            "traces/synthetic.ndjson": '{"fixture":"not real verification"}\n',
            "summary.md": "# SYNTHETIC interrupted initialization, not complete\n",
        }.items():
            path = self.assets / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        (self.assets / "harness/run.sh").chmod(0o755)
        identity = {
            "commit": self.revision, "original_commit": self.revision,
            "dirty": False, "source_mode": "snapshot",
        }
        write_json(self.manifest_path, {
            "version": 1, "kind": "ci-initialization", "target": "SYNTHETIC",
            "run_id": self.seed.name, "run_root": f"/old-work/ci/runs/{self.seed.name}",
            "validation_status": "UNVERIFIED", "pipeline_exit_code": 76,
            "source": {"before": identity, "after": identity},
            "assets": self.assets.relative_to(self.seed).as_posix(),
            "files_sha256": asset_hashes(self.assets), "missing_artifacts": [],
            "omitted_paths": ["traces/trace.ndjson"],
        })
        write_json(self.seed / "ci-input.json", {
            "version": 1, "target": self.target, "artifact": "/old-sources/not-mounted",
            "source": f"runs/{self.seed.name}/ci-source", "source_commit": self.revision,
            "snapshot_commit": self.snapshot, "dirty": False,
            "guidance": "SYNTHETIC historical guidance; retain as provenance, not results.",
            "previous": None,
        })
        self.arguments = {
            "seed_run": self.seed, "baseline": self.baseline,
            "manifest_sha256": self.manifest_digest(), "revision": self.revision,
            "target": self.target, "run_id": "SYNTHETIC-import",
        }
        self.token = "runs/SYNTHETIC-import/ci-published/SYNTHETIC-import"
        self.run = self.root / "runs/SYNTHETIC-import"
        self.publication = self.root / self.token

    def manifest_digest(self):
        return hashlib.sha256(self.manifest_path.read_bytes()).hexdigest()

    def import_fixture(self, **overrides):
        return import_baseline(self.root, **(self.arguments | overrides))

    def change_record(self, path, change):
        record = read_json(path)
        change(record)
        write_json(path, record)

    def test_explicit_import_is_audited_unverified_without_fabricating_completion(self):
        original = {
            str(path.relative_to(self.seed)): path.read_bytes()
            for path in self.seed.rglob("*") if path.is_file() and ".git" not in path.parts
        }
        with patch.object(CIStore, "publish", side_effect=AssertionError("import must not publish completion")):
            result = self.import_fixture()["current"]
        self.assertEqual(result["baseline_kind"], IMPORT_KIND)
        self.assertEqual(result["verdict"], "UNVERIFIED")
        self.assertFalse(result["verification_complete"])
        self.assertFalse(result["findings_finalized"])
        self.assertEqual(result["finding_counts"], {})
        self.assertEqual(result["origin"]["pipeline_exit_code"], 76)
        self.assertEqual(result["origin"]["manifest_sha256"], self.manifest_digest())
        self.assertEqual(result["origin"]["source_run_id"], self.seed.name)
        self.assertFalse((self.run / "ci-result.json").exists())
        self.assertFalse((self.publication / "model/ci-verdict.json").exists())
        self.assertFalse((self.seed / "ci-result.json").exists())
        self.assertFalse((self.seed / "current").exists())
        state = CIStore(self.root).snapshot(self.token)
        self.assertIn("UNVERIFIED", state["verification"])
        self.assertIsNone(state["checked_source_commit"])
        self.assertEqual(asset_hashes(self.publication / "model"), read_json(self.manifest_path)["files_sha256"])
        self.assertEqual((self.publication / "origin/ci-baseline.json").read_bytes(), self.manifest_path.read_bytes())
        self.assertEqual((self.publication / "origin/ci-input.json").read_bytes(), (self.seed / "ci-input.json").read_bytes())
        self.assertTrue((self.publication / "model/harness/run.sh").stat().st_mode & 0o111)
        for name, content in original.items():
            self.assertEqual((self.seed / name).read_bytes(), content)
        self.assertEqual(operate(self.root, "inspect")["current"], result)

    def test_cli_contract_writes_import_output(self):
        output = self.directory / "releases/internal/import.json"
        command = [
            sys.executable, str(Path(__file__).resolve().parents[1] / "store.py"),
            "import-baseline", "--ci-dir", str(self.root), "--output", str(output),
        ]
        for name, value in self.arguments.items():
            command.extend((f"--{name.replace('_', '-')}", str(value)))
        process = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertEqual(read_json(output)["current"]["token"], self.token)

    def test_idempotence_validates_full_import_and_provenance(self):
        first = self.import_fixture()
        before = asset_hashes(self.publication)
        self.assertEqual(self.import_fixture(), first)
        self.assertEqual(asset_hashes(self.publication), before)
        self.change_record(self.run / "ci-import.json", lambda value: value["origin"].update(pipeline_exit_code=0))
        with self.assertRaises(CIError):
            self.import_fixture()

    def test_source_is_independent_after_seed_disappears(self):
        self.import_fixture()
        shutil.rmtree(self.seed)
        state = CIStore(self.root).snapshot(self.token)
        source = self.root / state["source"]
        self.assertEqual(git(source, "rev-parse", "HEAD"), self.snapshot)
        self.assertEqual(git(source, "cat-file", "-t", self.revision), "commit")
        self.assertEqual(git(source, "rev-parse", "refs/specula/import-source"), self.revision)
        git(source, "fsck", "--full", "--no-dangling")
        self.assertFalse((source / ".git/objects/info/alternates").exists())
        self.assertEqual(operate(self.root, "inspect")["current"]["verdict"], "UNVERIFIED")

    def test_manifest_pin_source_and_target_mismatches_are_rejected(self):
        for changes in (
            {"manifest_sha256": "0" * 64}, {"revision": "0" * 40},
            {"target": "SYNTHETIC|other/project|Text|Different target"},
            {"baseline": "../escape.json"}, {"run_id": "../escape"},
            {"run_id": self.seed.name},
        ):
            with self.subTest(changes=changes), self.assertRaises(CIError):
                self.import_fixture(**changes)
        self.assertIsNone(CIStore(self.root).current_token())
        self.change_record(self.manifest_path, lambda value: value["source"]["after"].update(commit="0" * 40))
        with self.assertRaises(CIError):
            self.import_fixture(manifest_sha256=self.manifest_digest())

    def test_source_input_relation_and_actual_source_must_match(self):
        path = self.seed / "ci-input.json"
        original = path.read_bytes()
        for changes in (
            {"source": "runs/unrelated/ci-source"}, {"snapshot_commit": self.revision},
            {"dirty": True}, {"source_commit": "0" * 40},
        ):
            with self.subTest(changes=changes):
                path.write_bytes(original)
                self.change_record(path, lambda value: value.update(changes))
                with self.assertRaises(CIError):
                    self.import_fixture()
        path.write_bytes(original)
        (self.source / "fixture.txt").write_text("tampered synthetic seed\n")
        with self.assertRaises(CIError):
            self.import_fixture()
        self.assertIsNone(CIStore(self.root).current_token())

    def test_asset_mismatch_missing_harness_and_unlisted_assets_are_rejected(self):
        model = self.assets / "spec/base.tla"
        original = model.read_bytes()
        model.write_text("different synthetic model\n")
        with self.assertRaises(CIError):
            self.import_fixture()
        model.write_bytes(original)
        extra = self.assets / "spec/extra.tla"
        extra.write_text("unlisted synthetic asset\n")
        with self.assertRaises(CIError):
            self.import_fixture()
        extra.unlink()
        self.change_record(self.manifest_path, lambda value: value["files_sha256"].pop("harness/run.sh"))
        (self.assets / "harness/run.sh").unlink()
        with self.assertRaises(CIError):
            self.import_fixture(manifest_sha256=self.manifest_digest())
        self.assertIsNone(CIStore(self.root).current_token())

    def test_unsafe_paths_and_symlinks_are_rejected_even_when_omitted(self):
        self.change_record(self.manifest_path, lambda value: value["files_sha256"].update({"../outside": "0" * 64}))
        with self.assertRaises(CIError):
            self.import_fixture(manifest_sha256=self.manifest_digest())
        self.change_record(self.manifest_path, lambda value: value["files_sha256"].pop("../outside"))
        (self.assets / "traces/trace.ndjson").symlink_to("synthetic.ndjson")
        with self.assertRaises(CIError):
            self.import_fixture(manifest_sha256=self.manifest_digest())
        (self.assets / "traces/trace.ndjson").unlink()
        path = self.assets / "harness/run.sh"
        path.unlink()
        path.symlink_to(self.assets / "spec/base.tla")
        with self.assertRaises(CIError):
            self.import_fixture(manifest_sha256=self.manifest_digest())

    def test_inspection_rejects_model_state_audit_archive_and_source_tampering(self):
        self.import_fixture()
        for path, transform in (
            (self.publication / "model/spec/base.tla", lambda data: data + b"tampered\n"),
            (self.publication / "state.json", lambda data: data.replace(b'"UNVERIFIED"', b'"PASS"')),
            (self.run / "ci-import.json", lambda data: data.replace(b'"pipeline_exit_code": 76', b'"pipeline_exit_code": 0')),
            (self.publication / "origin/ci-input.json", lambda data: data + b" "),
            (self.publication / "origin/ci-baseline.json", lambda data: data + b" "),
            (self.run / "ci-source/fixture.txt", lambda data: data + b"tampered\n"),
            (self.run / "ci-source/.git/config", lambda data: data + b"\n# tampered\n"),
        ):
            with self.subTest(path=path):
                original = path.read_bytes()
                path.write_bytes(transform(original))
                with self.assertRaises(CIError):
                    operate(self.root, "inspect")
                path.write_bytes(original)
        self.assertEqual(operate(self.root, "inspect")["current"]["token"], self.token)

    def test_rehashing_modified_model_in_native_state_does_not_bypass_manifest(self):
        self.import_fixture()
        (self.publication / "model/spec/base.tla").write_text("tampered and rehashed\n")
        self.change_record(
            self.publication / "state.json",
            lambda value: value.update(files_sha256=asset_hashes(self.publication / "model")),
        )
        with self.assertRaises(CIError):
            operate(self.root, "inspect")

    def test_unrelated_current_and_partial_stores_are_never_replaced(self):
        token, _, _ = self.publish("SYNTHETIC-unrelated")
        with self.assertRaises(CIError):
            self.import_fixture()
        self.assertEqual(CIStore(self.root).current_token(), token)
        empty = self.directory / "different-private-ci"
        (empty / "runs/partial").mkdir(parents=True)
        with self.assertRaises(CIError):
            import_baseline(empty, **self.arguments)
        self.assertIsNone(CIStore(empty).current_token())

    def test_import_cannot_modify_the_original_seed_tree(self):
        for root in (self.seed, self.seed / "new-ci"):
            with self.subTest(root=root), self.assertRaises(CIError):
                import_baseline(root, **self.arguments)
        self.assertFalse((self.seed / ".lock").exists())
        self.assertFalse((self.seed / "runs").exists())
        self.assertFalse((self.seed / "new-ci").exists())

    def test_failure_before_native_validation_cannot_advance_current(self):
        with patch.object(CIStore, "snapshot", side_effect=CIError("SYNTHETIC validation rejection")):
            with self.assertRaises(CIError):
                self.import_fixture()
        self.assertIsNone(CIStore(self.root).current_token())
        with self.assertRaises(CIError):
            self.import_fixture()

    def test_import_is_neither_completed_initialization_nor_checked_ancestor(self):
        self.import_fixture()
        for operation, verdict in (("verify-initialization", "UNVERIFIED"), ("verify-initialization", "PASS"),
                                   ("promote", "UNVERIFIED"), ("promote", "PASS")):
            with self.subTest(operation=operation, verdict=verdict), self.assertRaises(CIError):
                operate(self.root, operation, token=self.token, revision=self.revision,
                        target=self.target, verdict=verdict)
        store = CIStore(self.root)
        store.acquire()
        try:
            state = store.current()
            self.assertFalse(matches_source(store, state, state["source_tree"], state["check_key"]))
            self.assertIsNone(inherit(store, self.source, self.revision, state["check_key"]))
        finally:
            store.close()
        write_json(self.run / "ci-result.json", {
            "run_id": "SYNTHETIC-import", "snapshot": self.token, "complete": True,
            "candidate": False, "verdict": "UNVERIFIED",
        })
        with self.assertRaises(CIError):
            operate(self.root, "inspect")

    def test_real_completed_native_candidate_can_promote_over_import(self):
        self.import_fixture()
        # Use the original source commit (not its native orphan snapshot) as parent.
        git(self.source, "checkout", "--quiet", "--detach", self.revision)
        token, revision, _ = self.publish(
            "SYNTHETIC-completed-update", previous=self.token,
            parent_source=self.source, candidate=True,
        )
        receipt = self.root / "runs/SYNTHETIC-completed-update/ci-result.json"
        self.change_record(receipt, lambda value: value.update(complete=False, exit_code=137))
        with self.assertRaises(CIError):
            operate(self.root, "promote", token=token, revision=revision, target=self.target, verdict="PASS")
        self.assertEqual(CIStore(self.root).current_token(), self.token)
        self.change_record(receipt, lambda value: (value.update(complete=True), value.pop("exit_code")))
        for _ in range(2):
            result = operate(self.root, "promote", token=token, revision=revision,
                             target=self.target, verdict="PASS")
            self.assertEqual(result["current"]["token"], token)

    def test_real_native_incremental_preparation_reuses_import_without_model_calls(self):
        self.import_fixture()
        git(self.source, "checkout", "--quiet", "--detach", self.revision)
        (self.source / "fixture.txt").write_text("SYNTHETIC descendant source, not verification\n")
        git(self.source, "add", "fixture.txt")
        git(self.source, "commit", "--quiet", "-m", "Synthetic descendant fixture")
        revision = git(self.source, "rev-parse", "HEAD")
        pipeline = CIPipeline()
        self.assertIsNone(pipeline.parse_args([
            "--incremental", "--ci-candidate", f"--ci-dir={self.root}",
            f"--artifact={self.source}", f"--revision={revision}", "--policy-retries=0",
            "--transient-resumes=0", self.target,
        ]))
        try:
            with patch.object(pipeline, "_phase", side_effect=AssertionError("no model calls")):
                self.assertIsNone(pipeline.resolve_run_dir(acquire_lock=True))
                names = pipeline.extract_names()
                pipeline.prepare_source_snapshots(names)
            self.assertEqual(pipeline.inputs["previous"], self.token)
            self.assertEqual(pipeline.inputs["source_commit"], revision)
            self.assertEqual(pipeline.inputs["old_model"], str(self.publication / "model"))
            self.assertIn("descendant source", (pipeline.run_dir / "source.diff").read_text())
            work = Path(pipeline.get_work_dir(names[0]))
            self.assertEqual((work / "spec/base.tla").read_bytes(), (self.assets / "spec/base.tla").read_bytes())
            self.assertEqual(
                (work / "spec/confirmation/MC-1/investigation.md").read_bytes(),
                (self.assets / "spec/confirmation/MC-1/investigation.md").read_bytes(),
            )
            self.assertFalse((work / "ci-verdict.json").exists())
            self.assertFalse((pipeline.run_dir / "ci-result.json").exists())
            self.assertEqual(CIStore(self.root).current_token(), self.token)
        finally:
            pipeline._release_run_lock()


if __name__ == "__main__":
    if not os.environ.get("TMPDIR", "").startswith("/work/"):
        raise SystemExit("Run synthetic imports only in the bounded runtime with /work scratch.")
    unittest.main()
