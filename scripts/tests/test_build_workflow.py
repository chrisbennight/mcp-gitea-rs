import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from textwrap import dedent

ROOT = Path(__file__).resolve().parents[2]
SMOKE_SCRIPT = ROOT / "scripts/smoke-image.sh"


class BuildWorkflowTests(unittest.TestCase):
    def test_tested_artifact_crosses_the_job_boundary_without_a_rebuild(self):
        build = (ROOT / ".github/workflows/build.yml").read_text()
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        for name in ["Prepare image artifact", "Smoke health endpoint", "Check independent MCP clients",
                     "Record validated identity", "Retain tested artifact"]:
            self.assertIn(name, build)
        positions = [build.index(name) for name in ["Prepare image artifact", "Smoke health endpoint",
                    "Check independent MCP clients", "Record validated identity", "Retain tested artifact"]]
        self.assertEqual(positions, sorted(positions))
        self.assertNotIn("inputs.publish", build)
        self.assertNotIn("packages: write", build)
        self.assertIn("packages: read", build)
        self.assertIn("${{ steps.candidate.outputs.image }}", build)
        self.assertIn("artifact-ids: ${{ needs.image.outputs.artifact_id }}", ci)
        publication = ci.split("  publish:\n")[1]
        self.assertNotIn("build.yml", publication)
        self.assertNotIn("docker build", publication)
        self.assertLess(publication.index("Verify publication evidence"), publication.index("Publish tested artifact"))

    def test_only_canonical_push_can_publish_after_all_required_checks(self):
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        before, publication = ci.split("  publish:\n")
        self.assertNotIn("packages: write", before)
        self.assertIn("packages: write", publication)
        self.assertIn("needs: [test, image, advisories]", publication)
        self.assertIn("github.event_name == 'push'", publication)
        self.assertIn("github.ref == 'refs/heads/main'", publication)
        self.assertIn("github.repository == 'chrisbennight/mcp-gitea-rs'", publication)
        self.assertIn("cancel-in-progress: false", ci)
        self.assertNotIn("pull_request_target", ci)

    def run_smoke(self, cleanup_fails):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            docker = root / "docker"
            log = root / "docker.log"
            docker.write_text(
                dedent(
                    """\
                    #!/bin/sh
                    printf '%s\\n' "$*" >> "$DOCKER_LOG"
                    if [ "$1" = rm ] && [ "$CLEANUP_FAILS" = 1 ]; then
                      exit 1
                    fi
                    """
                )
            )
            docker.chmod(0o755)
            environment = os.environ | {
                "PATH": f"{root}:{os.environ['PATH']}",
                "DOCKER_LOG": str(log),
                "CLEANUP_FAILS": "1" if cleanup_fails else "0",
            }
            result = subprocess.run(
                [SMOKE_SCRIPT, "registry.example/owner/image:revision", "smoke-test"],
                env=environment,
                cwd=ROOT,
                capture_output=True,
                text=True,
                check=False,
            )
            calls = log.read_text().splitlines() if log.exists() else []
        return result, calls

    def test_smoke_cleanup_fails_loudly(self):
        success, success_calls = self.run_smoke(cleanup_fails=False)
        failure, failure_calls = self.run_smoke(cleanup_fails=True)

        self.assertEqual(success.returncode, 0)
        self.assertIn("rm -f smoke-test", success_calls)
        self.assertEqual(failure.returncode, 1)
        self.assertIn("rm -f smoke-test", failure_calls)
        self.assertIn("failed to remove image smoke container", failure.stderr)
