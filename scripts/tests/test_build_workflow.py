import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from textwrap import dedent

ROOT = Path(__file__).resolve().parents[2]
SMOKE_SCRIPT = ROOT / "scripts/smoke-image.sh"


class BuildWorkflowTests(unittest.TestCase):


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
                    if [ "$1" = create ]; then
                      printf '%s\\n' synthetic-owned-cid
                    fi
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
        self.assertIn("rm -f -v synthetic-owned-cid", success_calls)
        self.assertEqual(failure.returncode, 1)
        self.assertIn("rm -f -v synthetic-owned-cid", failure_calls)
        self.assertIn("failed to remove image smoke container", failure.stderr)
