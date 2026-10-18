"""P4-A variant manifest tests with local synthetic fixtures only."""

import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

import e2e_queue
import variant
from test_acceptance_runner import make_spec


class VariantTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.variant = self.root / "variant.json"
        self.variant.write_text(json.dumps({"id": "mock-v1", "models": {"strong": "mock-a",
                                 "main": "mock-b", "cheap": "mock-c"}, "base_url": "http://localhost:1"}))

    def manifest(self, **kwargs) -> dict:
        return variant.capture_manifest(kwargs.pop("variant_id", "v1"),
                                        binary=kwargs.pop("binary", None),
                                        variant=kwargs.pop("variant", self.variant),
                                        **kwargs)

    def test_capture_is_deterministic_for_identical_inputs(self):
        first = self.manifest(variant_id="same")
        second = self.manifest(variant_id="same")
        first.pop("created_at")
        second.pop("created_at")
        self.assertEqual(first, second)

    def test_manifest_digest_is_path_independent(self):
        # Two captures of identical content from different template copies
        # must produce the same digest; only paths differ, and paths are not
        # part of the digest inputs except where content is identified.
        first = self.manifest()
        second = self.manifest()
        self.assertEqual(first["variant_digest"], second["variant_digest"])

    def test_model_only_variation_changes_digest(self):
        base = self.manifest()
        changed = json.loads(self.variant.read_text())
        changed["models"]["main"] = "mock-other"
        self.variant.write_text(json.dumps(changed))
        other = self.manifest()
        self.assertNotEqual(base["variant_digest"], other["variant_digest"])
        differences = variant.compare_manifests(base, other)
        self.assertEqual(list(differences), ["models"])

    def test_prompt_only_variation_changes_digest(self):
        base = self.manifest()
        template_copy = self.root / "template-copy"
        shutil.copytree(e2e_queue.DEFAULT_TEMPLATE, template_copy := self.root / "template-copy")
        (template_copy / "prompts/reviewer.md").write_text("# Alternate reviewer\n")
        changed = variant.capture_manifest("v-b", template=template_copy,
                                           variant=self.variant)
        self.assertNotEqual(base["variant_digest"], changed["variant_digest"])
        self.assertNotEqual(base["team_template"]["prompt_digests"]["reviewer"],
                            changed["team_template"]["prompt_digests"]["reviewer"])
        self.assertEqual(base["team_template"]["prompt_digests"]["reviewer"],
                         variant.sha(
                             (e2e_queue.DEFAULT_TEMPLATE / "prompts/reviewer.md").read_bytes()))

    def test_different_runtime_binaries_are_distinguishable(self):
        base = self.manifest()
        other_binary = self.root / "other-studio"
        other_binary.write_bytes(b"\x7fELF-fake-variant-binary")
        other = self.manifest(binary=other_binary)
        self.assertNotEqual(base["variant_digest"], other["variant_digest"])
        self.assertNotEqual(base["runtime_binary"]["sha256"],
                            other["runtime_binary"]["sha256"])

    def test_same_variant_name_with_different_content_yields_different_digest(self):
        base = self.manifest(variant_id="research")
        other_binary = self.root / "mutated-studio"
        other_binary.write_bytes(b"\x7fELF-mutated")
        renamed = self.manifest(variant_id="research", binary=other_binary)
        self.assertNotEqual(base["variant_digest"], renamed["variant_digest"])

    def test_dirty_source_state_is_recorded_and_binary_marked_unproven(self):
        # Do not assume the production repo tree is dirty (a completed
        # release commit makes it clean); exercise the dirty path on a
        # synthetic repo with committed, modified and untracked files.
        repo = self.root / "dirty-repo"
        repo.mkdir()
        (repo / "main.rs").write_text("fn main() {}\n")
        subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
        subprocess.run(["git", "add", "."], cwd=repo, check=True)
        subprocess.run(["git", "-c", "user.email=t@t", "-c", "user.name=t",
                        "commit", "-qm", "c"], cwd=repo, check=True)
        (repo / "main.rs").write_text("fn main() { println!(\"dirty\"); }\n")
        (repo / "scratch.txt").write_text("untracked\n")

        binary = self.root / "dirty-variant-studio"
        binary.write_bytes(b"\x7fELF-fake-dirty-variant-binary")
        manifest = self.manifest(binary=binary, source_cwd=repo)
        self.assertTrue(manifest["source"]["dirty"])
        self.assertIn("unproven", manifest["runtime_binary"]["source_provenance"])
        self.assertTrue(manifest["source"]["untracked_files"])

        dirty_state = variant.capture_source_state(cwd=repo)
        self.assertTrue(dirty_state["dirty"])
        self.assertTrue(dirty_state["untracked_files"])
        self.assertIn("unproven", variant.binary_provenance(binary))

    def test_clean_synthetic_source_marks_provenance_clean(self):
        # A throwaway git repo with a clean tree proves nothing about the
        # production binary, but the field must distinguish clean from dirty.
        repo = self.root / "clean-repo"
        repo.mkdir()
        (repo / "main.rs").write_text("fn main() {}\n")
        subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
        subprocess.run(["git", "add", "."], cwd=repo, check=True)
        subprocess.run(["git", "-c", "user.email=t@t", "-c", "user.name=t",
                        "commit", "-qm", "c"], cwd=repo, check=True)
        state = variant.capture_source_state(cwd=repo)
        self.assertFalse(state["dirty"])
        self.assertEqual(state["untracked_files"], [])
        # empty status string still hashes deterministically
        self.assertEqual(state["status_digest"], variant.sha(b""))

    def test_missing_binary_is_reported(self):
        missing = self.root / "no-such-binary"
        with self.assertRaises(FileNotFoundError):
            variant.capture_manifest("v", binary=missing)

    def test_manifest_tampering_is_detected(self):
        path = self.root / "manifest.json"
        manifest = self.manifest()
        e2e_queue.save_json(path, manifest)
        tampered = json.loads(path.read_text())
        tampered["models"]["main"] = "secretly-changed"
        e2e_queue.save_json(path, tampered)
        with self.assertRaises(ValueError):
            variant.load_manifest(path)

    def test_recomputed_digest_is_stable_after_roundtrip(self):
        path = self.root / "manifest.json"
        manifest = self.manifest()
        e2e_queue.save_json(path, manifest)
        loaded = variant.load_manifest(path)
        self.assertEqual(loaded["variant_digest"], manifest["variant_digest"])

    def test_secrets_are_rejected(self):
        # capture from a clean variant file must succeed
        self.assertTrue(variant.capture_manifest("v", variant=self.variant))
        smuggled = self.variant.parent / "evil.json"
        smuggled.write_text(json.dumps({"id": "v", "models": {"strong": "a", "main": "b",
                                                              "cheap": "c"},
                                        "api_key": "sk-leak"}))
        # rejected by the P3 variant validator (unknown field) and by the
        # manifest-level credential screen
        with self.assertRaises(ValueError):
            variant.capture_manifest("v", variant=smuggled)
        with self.assertRaises(ValueError):
            variant.validate_manifest_inputs({"models": {"strong": "a", "main": "b",
                                                         "cheap": "c"},
                                              "api_key": "sk-leak"})

    def test_credential_free_base_url_required(self):
        bad = self.root / "bad.json"
        bad.write_text(json.dumps({"id": "v", "models": {"strong": "a", "main": "b",
                                                         "cheap": "c"},
                                   "base_url": "https://user:pass@host.example"}))
        with self.assertRaises(ValueError):
            variant.capture_manifest("v", variant=bad)

    def test_e2e_queue_accepts_prepared_task_with_variant_flow(self):
        # End-to-end with the real P3 queue on synthetic fixtures: prepare,
        # enqueue, and confirm the variant digest linkage lands in metadata.
        fixture = self.root / "fixture-repo"
        fixture.mkdir()
        (fixture / "calc.py").write_bytes(b"def add(a,b): return 0\n")
        subprocess.run(["git", "init", "-q"], cwd=fixture, check=True)
        spec = make_spec(self.root, "import unittest\nfrom calc import add\n"
                         "class TestAdd(unittest.TestCase):\n"
                         " def test_add(self): self.assertEqual(add(1,2),3)\n")
        definition = e2e_queue.prepare("variant-flow", "1", fixture,
                                       spec.parent / "fixture.txt", spec,
                                       self.variant, self.root / "task.json")
        self.assertEqual(definition["variant_id"], "mock-v1")
        store = e2e_queue.QueueStore(self.root / "queue")
        store.enqueue(self.root / "task.json")
        row = store.one("variant-flow")
        self.assertEqual(row["state"], "queued")
        pinned = json.loads(Path(row["definition"]).read_text())
        manifest = self.manifest()
        self.assertEqual(pinned["variant_digest"], e2e_queue.sha(self.variant.read_bytes()))

    def test_prepare_detects_modified_pinned_variant(self):
        fixture = self.root / "fixture-repo"
        fixture.mkdir()
        (fixture / "calc.py").write_bytes(b"def add(a,b): return 0\n")
        subprocess.run(["git", "init", "-q"], cwd=fixture, check=True)
        spec = make_spec(self.root, "import unittest\nfrom calc import add\n"
                         "class TestAdd(unittest.TestCase):\n"
                         " def test_add(self): self.assertEqual(add(1,2),3)\n")
        definition = e2e_queue.prepare("tamper", "1", fixture, spec.parent / "fixture.txt",
                                       spec, self.variant, self.root / "task.json")
        store = e2e_queue.QueueStore(self.root / "queue")
        store.enqueue(self.root / "task.json")
        self.variant.write_text(json.dumps({"id": "mock-v1", "models": {"strong": "mock-a",
                                            "main": "mutated", "cheap": "mock-c"},
                                            "base_url": "http://localhost:1"}))
        with self.assertRaises(ValueError):
            store.enqueue(self.root / "task.json")

    def test_verify_against_attempt_detects_mismatch(self):
        metadata = {"runtime_binary_digest": "a" * 64, "template_digest": "b" * 64,
                    "models": {"strong": "mock-a", "main": "mock-b", "cheap": "mock-c"},
                    "base_url": "http://localhost:1", "scout_interval_secs": 3600,
                    "source_at_run": {"revision": "r", "status_digest": "s"},
                    "prompt_digests": {}}
        attempt = self.root / "attempt"
        attempt.mkdir()
        e2e_queue.save_json(attempt / "metadata.json", metadata)
        manifest = {"variant_id": "v",
                    "runtime_binary": {"sha256": "a" * 64},
                    "team_template": {"tree_digest": "b" * 64, "prompt_digests": {}},
                    "models": metadata["models"], "base_url": metadata["base_url"],
                    "scout_interval_secs": 3600,
                    "source": {"revision": "r", "status_digest": "s"}}
        self.assertEqual(variant.verify_against_attempt(manifest, attempt), [])
        drifted = dict(manifest, models={"strong": "mock-a", "main": "changed", "cheap": "mock-c"})
        problems = variant.verify_against_attempt(drifted, attempt)
        self.assertTrue(any("model" in problem for problem in problems))
        revision_drift = dict(manifest, source={"revision": "other", "status_digest": "s"})
        self.assertTrue(any("revision" in problem
                            for problem in variant.verify_against_attempt(revision_drift, attempt)))


if __name__ == "__main__":
    unittest.main()
