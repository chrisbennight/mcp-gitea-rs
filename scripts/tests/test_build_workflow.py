import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from textwrap import dedent


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "build.yml"
SMOKE_SCRIPT = ROOT / "scripts" / "smoke-image.sh"
BUILD_SCRIPT = ROOT / "build-docker.sh"


class BuildWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = WORKFLOW.read_text()

    def step(self, name):
        marker = f"      - name: {name}\n"
        start = self.source.index(marker)
        next_step = self.source.find("\n      - name:", start + len(marker))
        end = len(self.source) if next_step == -1 else next_step
        return dedent(self.source[start:end])

    def run_step(self, name, manifest_state, github_ref):
        step = self.step(name)
        run_marker = "  run: |\n"
        script = dedent(step[step.index(run_marker) + len(run_marker) :])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            docker = root / "docker"
            log = root / "docker.log"
            docker.write_text(
                dedent(
                    """\
                    #!/bin/sh
                    printf '%s\\n' "$*" >> "$DOCKER_LOG"
                    if [ "$1" = login ]; then
                      cat >/dev/null
                    elif [ "$1 $2" = "manifest inspect" ]; then
                      case "$MANIFEST_STATE" in
                        exists) exit 0 ;;
                        missing_unknown) echo "manifest unknown" >&2; exit 1 ;;
                        missing_no_such) echo "no such manifest: $3" >&2; exit 1 ;;
                        error) echo "registry unavailable" >&2; exit 1 ;;
                      esac
                    fi
                    """
                )
            )
            docker.chmod(0o755)
            environment = os.environ | {
                "PATH": f"{root}:{os.environ['PATH']}",
                "DOCKER_LOG": str(log),
                "MANIFEST_STATE": manifest_state,
                "IMAGE": "registry.example/owner/image",
                "GITHUB_SHA": "a" * 40,
                "GITHUB_REF": github_ref,
                "GITHUB_RUN_ID": "123",
                "GITHUB_RUN_ATTEMPT": "1",
                "REGISTRY_USER": "test-user",
                "REGISTRY_TOKEN": "test-token",
            }
            result = subprocess.run(
                ["bash", "-eu", "-o", "pipefail", "-c", script],
                env=environment,
                cwd=ROOT,
                capture_output=True,
                text=True,
                check=False,
            )
            calls = log.read_text().splitlines()
        return result, calls

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
            calls = log.read_text().splitlines()
        return result, calls

    def test_immutable_image_is_verified_and_smoked_before_publication(self):
        build = self.step("Build image")
        verify = self.step("Verify image architecture")
        smoke = self.step("Smoke health endpoint")
        publish = self.step("Publish image")
        positions = [
            self.source.index(f"- name: {name}")
            for name in (
                "Build image",
                "Verify image architecture",
                "Smoke health endpoint",
                "Publish image",
            )
        ]

        self.assertEqual(positions, sorted(positions))
        revision = '"${IMAGE}:sha-${GITHUB_SHA}"'
        # Asserted as separate properties rather than one contiguous command:
        # the build now forwards the crate index argument, which sits between
        # `docker build` and these flags. What this test is about is that the
        # build is amd64 and carries the revision tag, not argument order.
        self.assertIn("docker build", build)
        self.assertIn("--platform linux/amd64", build)
        self.assertIn(f"--tag {revision}", build)
        self.assertNotIn("--tag \"${IMAGE}:latest\"", build)
        self.assertIn("docker image inspect", verify)
        self.assertIn(revision, verify)
        self.assertIn("scripts/smoke-image.sh", smoke)
        self.assertIn(revision, smoke)
        self.assertIn('revision="${IMAGE}:sha-${GITHUB_SHA}"', publish)
        self.assertIn('docker push "$revision"', publish)
        self.assertNotIn("short_sha", self.source)

    def test_ci_and_local_builds_are_amd64_only(self):
        verify = self.step("Verify image architecture")

        self.assertIn("docker image inspect --format '{{.Architecture}}'", verify)
        self.assertIn('test "$architecture" = amd64', verify)
        self.assertIn('"$@" --platform linux/amd64 .', BUILD_SCRIPT.read_text())

    def test_revision_publication_is_serialized_and_never_rebinds(self):
        publish = self.step("Publish image")

        self.assertIn('docker manifest inspect "$revision"', publish)
        self.assertIn('"immutable revision already exists; refusing to rebind it"', publish)
        self.assertIn('[ "$inspection" = "manifest unknown" ]', publish)
        self.assertIn('[ "$inspection" = "no such manifest: $revision" ]', publish)
        self.assertIn('docker push "$revision"', publish)

    def test_pull_request_smokes_do_not_block_publication(self):
        group = (
            "group: ${{ github.event_name == 'pull_request' && github.run_id "
            "|| 'mcp-gitea-rs-image-publication' }}"
        )

        caller = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn(group, caller)
        self.assertIn("cancel-in-progress: false", caller)

    def test_publication_behavior_preserves_revision_immutability(self):
        revision = f"registry.example/owner/image:sha-{'a' * 40}"
        cases = (
            ("exists", "refs/tags/v1.0.0", 0, [], []),
            (
                "exists",
                "refs/heads/main",
                0,
                [],
                [
                    "buildx imagetools create --prefer-index=false "
                    f"--tag registry.example/owner/image:latest {revision}"
                ],
            ),
            (
                "missing_unknown",
                "refs/heads/main",
                0,
                [revision],
                [
                    "buildx imagetools create --prefer-index=false "
                    f"--tag registry.example/owner/image:latest {revision}"
                ],
            ),
            (
                "missing_no_such",
                "refs/heads/main",
                0,
                [revision],
                [
                    "buildx imagetools create --prefer-index=false "
                    f"--tag registry.example/owner/image:latest {revision}"
                ],
            ),
            ("error", "refs/heads/main", 1, [], []),
        )

        for state, ref, expected_returncode, expected_pushes, expected_aliases in cases:
            with self.subTest(state=state, ref=ref):
                result, calls = self.run_step("Publish image", state, ref)
                pushes = [call.removeprefix("push ") for call in calls if call.startswith("push ")]
                aliases = [call for call in calls if call.startswith("buildx imagetools create ")]
                self.assertEqual(result.returncode, expected_returncode)
                self.assertEqual(pushes, expected_pushes)
                self.assertEqual(aliases, expected_aliases)

    def test_latest_advances_only_from_main_after_smoke(self):
        publish = self.step("Publish image")

        guarded_latest = (
            '    if [ "$GITHUB_REF" = refs/heads/main ]; then\n'
            '      if [ "$revision_created" = 0 ]; then\n'
            '        docker pull "$revision"\n'
            "        scripts/smoke-image.sh \\\n"
        )
        self.assertIn(guarded_latest, publish)
        self.assertIn('"mcp-gitea-rs-remote-smoke-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"', publish)
        self.assertIn(
            "docker buildx imagetools create --prefer-index=false \\\n",
            publish,
        )
        self.assertIn('--tag "${IMAGE}:latest" "$revision"', publish)
        self.assertNotIn('docker push "${IMAGE}:latest"', publish)

    def test_publication_uses_scoped_github_token_only_on_trusted_push(self):
        publish = self.step("Publish image")
        caller = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("if: inputs.publish", publish)
        self.assertIn("REGISTRY_TOKEN: ${{ secrets.GITHUB_TOKEN }}", publish)
        self.assertIn("docker login ghcr.io", publish)
        self.assertIn("github.event_name == 'push'", caller)
        self.assertIn("github.ref == 'refs/heads/main'", caller)
        self.assertIn("github.repository == 'chrisbennight/mcp-gitea-rs'", caller)
        before_publish, publication = caller.split("  publish:\n")
        self.assertNotIn("packages: write", before_publish)
        self.assertIn("packages: write", publication)
        self.assertIn("needs: [test, image]", publication)
        self.assertNotIn("pull_request_target", caller)

    def test_smoke_cleanup_fails_loudly(self):
        success, success_calls = self.run_smoke(cleanup_fails=False)
        failure, failure_calls = self.run_smoke(cleanup_fails=True)

        self.assertEqual(success.returncode, 0)
        self.assertIn("rm -f smoke-test", success_calls)
        self.assertEqual(failure.returncode, 1)
        self.assertIn("rm -f smoke-test", failure_calls)
        self.assertIn("failed to remove image smoke container", failure.stderr)

    def test_github_builds_need_no_private_registry_or_action(self):
        build = self.step("Build image")
        self.assertNotIn("CRATES_INDEX_URL", build)
        self.assertNotIn("infisical", self.source.lower())
        self.assertNotIn("cacahuate.org", self.source)
        self.assertIn("ghcr.io/chrisbennight/mcp-gitea-rs", build)



if __name__ == "__main__":
    unittest.main()
