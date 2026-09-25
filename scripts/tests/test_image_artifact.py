import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location("image_artifact", SCRIPTS / "image_artifact.py")
artifact = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(artifact)


class ImageArtifactTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.archive = self.directory / "image.oci.tar"
        self.archive.write_bytes(b"synthetic OCI fixture")
        self.ctx = dict(zip(artifact.CONTEXT_KEYS, [artifact.REPOSITORY, "push", "refs/heads/main", "a" * 40, "123", "1"]))
        self.env = patch.dict(os.environ, self.ctx | {"GITHUB_STEP_SUMMARY": str(self.directory / "summary")})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.config = json.dumps({"architecture": "amd64", "os": "linux", "config": {
            "Labels": {"org.opencontainers.image.revision": self.ctx["GITHUB_SHA"]}}}).encode()
        self.manifest = json.dumps({"config": {"digest": artifact.digest(self.config)}}).encode()

    def inspect(self, argv, **kwargs):
        self.assertEqual(argv[:2], ["skopeo", "inspect"])
        return self.config if "--config" in argv else self.manifest

    def receipt(self):
        with patch.object(artifact, "command", side_effect=self.inspect):
            evidence = {"context": self.ctx, **artifact.identity(self.archive, self.ctx), "checks": artifact.CHECKS}
        (self.directory / "validated.json").write_text(json.dumps(evidence))
        return evidence

    def test_archive_manifest_and_configuration_are_all_bound_to_receipt(self):
        self.receipt()
        with patch.object(artifact, "command", side_effect=self.inspect):
            artifact.verify(self.directory)
            self.archive.write_bytes(b"different image")
            with self.assertRaisesRegex(artifact.ArtifactError, "mismatched"):
                artifact.verify(self.directory)

    def test_preparation_compares_configuration_across_docker_storage_backends(self):
        for mismatch in [False, True]:
            directory = self.directory / str(mismatch)
            image_id = "sha256:" + "c" * 64
            def command(argv, **kwargs):
                if argv[:3] == ["docker", "buildx", "build"]:
                    (directory / "image.oci.tar").write_bytes(b"synthetic OCI fixture")
                elif argv[:3] == ["docker", "image", "inspect"]:
                    return image_id.encode()
                elif argv[:2] == ["skopeo", "inspect"]:
                    if argv[-1].startswith("docker-daemon:"):
                        self.assertEqual(argv[-1], "docker-daemon:" + image_id)
                        return b"changed configuration" if mismatch else self.config
                    return self.inspect(argv)
            output = self.directory / "output"
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}), patch.object(artifact, "command", side_effect=command), patch.object(artifact, "remote_digest", return_value=None):
                if mismatch:
                    with self.assertRaisesRegex(artifact.ArtifactError, "loaded test image"):
                        artifact.prepare(directory)
                    self.assertFalse((directory / "candidate.json").exists())
                else:
                    artifact.prepare(directory)
                    self.assertEqual(output.read_text(), "image=" + image_id + "\n")

    def test_missing_stale_and_incomplete_evidence_fail_before_publication(self):
        for change in ["missing", "run", "attempt", "source", "checks", "manifest"]:
            with self.subTest(change=change):
                evidence = self.receipt()
                if change == "missing":
                    (self.directory / "validated.json").unlink()
                else:
                    if change in ["run", "attempt", "source"]:
                        key = {"run": "GITHUB_RUN_ID", "attempt": "GITHUB_RUN_ATTEMPT", "source": "GITHUB_SHA"}[change]
                        evidence["context"] = self.ctx | {key: "9"}
                    elif change == "checks":
                        evidence["checks"] = ["health"]
                    else:
                        evidence["manifest_digest"] = "sha256:" + "b" * 64
                    (self.directory / "validated.json").write_text(json.dumps(evidence))
                with patch.object(artifact, "command", side_effect=self.inspect), patch.object(artifact, "remote_digest") as remote:
                    with self.assertRaises((artifact.ArtifactError, FileNotFoundError)):
                        artifact.publish(self.directory)
                    remote.assert_not_called()

    def test_pull_requests_dispatch_forks_and_untrusted_refs_cannot_publish(self):
        self.receipt()
        for changed in [{"GITHUB_EVENT_NAME": "pull_request"}, {"GITHUB_EVENT_NAME": "workflow_dispatch"},
                        {"GITHUB_REPOSITORY": "someone/fork"}, {"GITHUB_REF": "refs/heads/topic"}]:
            with patch.dict(os.environ, changed), patch.object(artifact, "command") as command:
                with self.assertRaisesRegex(artifact.ArtifactError, "canonical"):
                    artifact.publish(self.directory)
                command.assert_not_called()

    def test_configuration_must_match_architecture_source_and_digest(self):
        original = self.config
        for changed in [{"architecture": "arm64"}, {"config": {}}, {"extra": "changed bytes"}]:
            self.config = json.dumps(json.loads(original) | changed).encode()
            with patch.object(artifact, "command", side_effect=self.inspect):
                with self.assertRaises(artifact.ArtifactError):
                    artifact.identity(self.archive, self.ctx)

    def test_published_digest_must_equal_validated_digest_and_collisions_prevent_writes(self):
        evidence = self.receipt()
        expected = evidence["manifest_digest"]
        for observed, should_pass in [([None, expected, expected], True),
                                      (["sha256:" + "b" * 64], False),
                                      ([None, "sha256:" + "b" * 64], False)]:
            calls = []
            def command(argv, **kwargs):
                if argv[1] == "inspect":
                    return self.inspect(argv)
                calls.append(argv)
            with patch.object(artifact, "command", side_effect=command), patch.object(artifact, "remote_digest", side_effect=observed):
                if should_pass:
                    artifact.publish(self.directory)
                    self.assertEqual(len(calls), 2)
                    self.assertTrue(all("--preserve-digests" in call for call in calls))
                else:
                    with self.assertRaises(artifact.ArtifactError):
                        artifact.publish(self.directory)
                    self.assertLessEqual(len(calls), 1)
                    if len(observed) == 1:
                        self.assertEqual(calls, [])

    def test_version_conflict_blocks_revision_publication_and_version_never_advances_latest(self):
        version = artifact.tomllib.loads((artifact.ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
        self.ctx["GITHUB_REF"] = "refs/tags/v" + version
        with patch.dict(os.environ, self.ctx):
            expected = self.receipt()["manifest_digest"]
            with patch.object(artifact, "command", side_effect=self.inspect), patch.object(artifact, "remote_digest", side_effect=[None, "conflict"]):
                with self.assertRaisesRegex(artifact.ArtifactError, "immutable"):
                    artifact.publish(self.directory)
            with patch.object(artifact, "command", side_effect=self.inspect), patch.object(artifact, "remote_digest", return_value=expected) as remote:
                artifact.publish(self.directory)
                self.assertFalse(any(":latest" in call.args[0] for call in remote.call_args_list))

    def test_registry_failure_is_not_absence_and_has_no_automatic_retry(self):
        for message, missing in [(b"manifest unknown", True), (b"unauthorized", False), (b"timeout", False)]:
            with patch.object(artifact.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, b"", message)) as run:
                if missing:
                    self.assertIsNone(artifact.remote_digest("registry.example/image:tag"))
                else:
                    with self.assertRaises(artifact.ArtifactError):
                        artifact.remote_digest("registry.example/image:tag")
                run.assert_called_once()


if __name__ == "__main__":
    unittest.main()
