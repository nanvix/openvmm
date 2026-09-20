"""Synthetic host wiring for explicit unverified imports; no model calls."""

from contextlib import redirect_stderr
import io
from pathlib import Path
import unittest
from unittest.mock import patch

import import_baseline
import release
import test_release


class ImportBackend(test_release.FakeBackend):
    def invoke(self, mode, arguments, **kwargs):
        if mode != "exec" or arguments[2] != "import-baseline":
            return super().invoke(mode, arguments, **kwargs)
        self.calls.append(("import-baseline", arguments))
        values = dict(zip(arguments[3::2], arguments[4::2]))
        run_id = values["--run-id"]
        imported = {
            "token": f"runs/{run_id}/ci-published/SYNTHETIC-import",
            "source_commit": values["--revision"], "target": values["--target"],
            "run_id": run_id, "verdict": "UNVERIFIED",
            "baseline_kind": "imported_unverified", "verification_complete": False,
            **self.config.get("_import_override", {}),
        }
        self.config.setdefault("_currents", {})[str(self.work)] = imported
        self.config.setdefault("_publications", {})[imported["token"]] = imported
        output = self.work / Path(values["--output"]).relative_to("/work")
        release.runtime.write_json(output, {"current": imported})
        record = {"status": self.config.get("_import_status", "command_success")}
        receipt = self.work / "runtime/SYNTHETIC-import/status.json"
        receipt.parent.mkdir(parents=True, exist_ok=True)
        release.runtime.write_json(receipt, record)
        return self.config.get("_import_code", 0), record, str(receipt)


class ImportBaselineTests(unittest.TestCase):
    def setUp(self):
        self.fixture = test_release.ReleaseTests("test_missing_baseline_never_implicitly_initializes")
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.config = self.fixture.config
        self.work = Path(self.config["work"])
        self.seed = self.work / "ci/runs/SYNTHETIC-interrupted"
        self.baseline = "ci-init/baselines/SYNTHETIC-frozen/ci-baseline.json"
        manifest = self.seed / self.baseline
        manifest.parent.mkdir(parents=True)
        release.runtime.write_json(self.seed / "ci-input.json", {
            "source_commit": self.fixture.a, "target": self.config["target"],
        })
        release.runtime.write_json(manifest, {
            "version": 1, "validation_status": "UNVERIFIED", "pipeline_exit_code": 76,
            "note": "SYNTHETIC HOST WIRING ONLY; native import validates actual artifacts separately",
        })
        self.manifest_hash = release.digest(manifest)
        self.original = (self.seed / "ci-input.json").read_bytes()
        # Backend fixtures share nested state even when production copies config.
        self.config.update(_calls=[], _currents={}, _publications={})

    def adopt(self, **kwargs):
        return import_baseline.adopt(
            self.config, seed_run=self.seed, baseline=self.baseline,
            manifest_sha256=kwargs.get("manifest_sha256", self.manifest_hash),
            request_id=kwargs.get("request_id", "SYNTHETIC-adoption"),
            backend_factory=ImportBackend, bundle_directory=self.fixture.control,
        )

    def test_import_is_explicit_input_not_a_completed_verification(self):
        with (
            patch.object(release, "prerequisites", side_effect=AssertionError("No model authorization needed for offline import")),
            patch.object(release, "source_for", side_effect=AssertionError("Offline import must not fetch or rewrite the historical source pin")),
        ):
            code, public, record = self.adopt()
        self.assertEqual(code, 0)
        self.assertFalse(record["complete"])
        self.assertFalse(record["verification_complete"])
        self.assertEqual(record["verdict"], "UNVERIFIED")
        self.assertEqual(record["status"], "baseline_imported_unverified")
        self.assertEqual(release.active_work(self.work), self.work / "initializations/SYNTHETIC-adoption")
        self.assertIn("explicitly imported, UNVERIFIED", (public / "summary.md").read_text())
        self.assertEqual((self.seed / "ci-input.json").read_bytes(), self.original)
        self.assertFalse(list(self.work.rglob("ci-result.json")))
        self.assertFalse(Path(self.config["authorization"]).exists())
        self.assertFalse(any(name == "native" for name, _ in self.config["_calls"]))

    def test_import_removes_only_initialization_blocker(self):
        self.adopt()
        code, _, report = self.fixture.run_request(
            self.fixture.request("preflight", request_id="after-import"),
            blockers=["provider approval missing"],
        )
        self.assertEqual(code, 3)
        self.assertEqual(report["status"], "provider_approval_required")
        self.assertEqual(report["blockers"], ["provider approval missing"])
        self.assertEqual(report["baseline_kind"], "imported_unverified")
        self.assertFalse(report["baseline_verification_complete"])
        self.assertIsNotNone(report["previous"])

    def test_imported_base_is_used_by_incremental_not_initialization(self):
        _, _, imported = self.adopt()
        code, _, updated = self.fixture.run_request(self.fixture.request(request_id="next-version"))
        self.assertEqual(code, 0)
        self.assertEqual(updated["previous"], imported["snapshot"])
        arguments = next(args for name, args in self.config["_calls"] if name == "native")
        self.assertIn("--incremental", arguments)
        self.assertNotIn("--ci-init", arguments)
        run = Path(updated["native_work"]) / "ci/runs" / updated["native_run"]
        self.assertIn("previous model was explicitly imported", release.read_json(run / "ci-input.json")["guidance"])

    def test_import_never_replaces_an_existing_current(self):
        self.fixture.baseline()
        code, _, report = self.adopt()
        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "already_initialized")
        self.assertEqual(release.active_work(self.work), self.work)
        self.assertFalse(any(name == "import-baseline" for name, _ in self.config["_calls"]))

    def test_failed_or_mismatched_import_cannot_select_native_work(self):
        for overrides in (
            {"_import_status": "wrapper_oom", "_import_code": 137},
            {"_import_override": {"verification_complete": True}},
            {"_import_override": {"source_commit": self.fixture.b}},
        ):
            with self.subTest(overrides=overrides):
                self.config.update(overrides)
                code, _, report = self.adopt()
                self.assertEqual(code, 1)
                self.assertIn(report["status"], {"import_failed", "invalid_import"})
                self.assertFalse((self.work / "releases/active-work.json").exists())
                for key in overrides:
                    self.config.pop(key)

    def test_same_import_is_idempotent_but_cannot_rewind_completed_incremental(self):
        _, _, first = self.adopt()
        code, _, second = self.adopt()
        self.assertEqual(code, 0)
        self.assertEqual(first["snapshot"], second["snapshot"])
        self.fixture.run_request(self.fixture.request(request_id="next-version"))
        code, _, report = self.adopt()
        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "already_initialized")

    def test_changed_manifest_and_symlink_are_rejected_before_import(self):
        with self.assertRaisesRegex(release.ReleaseError, "hash differs"):
            self.adopt(manifest_sha256="0" * 64)
        alias = self.seed.parent / "SYNTHETIC-alias"
        alias.symlink_to(self.seed, target_is_directory=True)
        self.seed = alias
        with self.assertRaisesRegex(release.ReleaseError, "symlink"):
            self.adopt()
        self.assertFalse(self.config["_calls"])

    def test_request_identity_cannot_change_and_original_report_is_preserved(self):
        _, public, _ = self.adopt()
        original = (public / "result.json").read_bytes()
        self.config["environment_id"] = "changed-environment"
        code, rejected, report = self.adopt()
        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "request_identity_changed")
        self.assertNotEqual(public, rejected)
        self.assertEqual((public / "result.json").read_bytes(), original)

    def test_cli_requires_explicit_unverified_acknowledgement(self):
        arguments = [
            "--seed-run", str(self.seed), "--baseline", self.baseline,
            "--manifest-sha256", self.manifest_hash, "--request-id", "SYNTHETIC-adoption",
        ]
        with patch.object(release, "load_config") as load, redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as error:
                import_baseline.main(arguments)
        self.assertEqual(error.exception.code, 2)
        load.assert_not_called()
        self.assertNotIn("import-baseline", release.MODES)


if __name__ == "__main__":
    unittest.main()
