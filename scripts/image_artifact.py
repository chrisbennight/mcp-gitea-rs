"""Build, validate, and promote one immutable OCI image artifact."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

from release_version import release_tag

ROOT = Path(__file__).resolve().parents[1]
REPOSITORY = "chrisbennight/mcp-gitea-rs"
IMAGE = "ghcr.io/" + REPOSITORY
CONTEXT_KEYS = (
    "GITHUB_REPOSITORY", "GITHUB_EVENT_NAME", "GITHUB_REF", "GITHUB_SHA",
    "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT",
)
CHECKS = ["architecture", "health", "python_mcp", "typescript_mcp"]


class ArtifactError(Exception):
    """Artifact identity or publication precondition was not established."""


def command(argv, *, capture=False):
    result = subprocess.run(argv, cwd=ROOT, capture_output=capture, check=False, timeout=1800)
    if result.returncode:
        raise ArtifactError(f"{argv[0]} {argv[1]} failed")
    return result.stdout if capture else None


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def file_digest(path):
    with path.open("rb") as stream:
        return "sha256:" + hashlib.file_digest(stream, "sha256").hexdigest()


def context():
    result = {key: os.environ[key] for key in CONTEXT_KEYS}
    if not re.fullmatch(r"[0-9a-f]{40}", result["GITHUB_SHA"]):
        raise ArtifactError("invalid source revision")
    for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT"):
        if not re.fullmatch(r"[1-9][0-9]*", result[key]):
            raise ArtifactError("invalid workflow identity")
    return result


def trusted(ctx):
    ref = ctx["GITHUB_REF"]
    return (ctx["GITHUB_REPOSITORY"] == REPOSITORY
            and ctx["GITHUB_EVENT_NAME"] == "push"
            and (ref == "refs/heads/main" or ref.startswith("refs/tags/v")))


def version_tag(ctx):
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    return release_tag(ctx["GITHUB_REF"], version)


def remote_digest(reference):
    result = subprocess.run(
        ["skopeo", "inspect", "--raw", "docker://" + reference],
        capture_output=True, check=False, timeout=120,
    )
    if result.returncode == 0:
        return digest(result.stdout)
    # A transport or authorization failure must never be mistaken for absence.
    error = result.stderr.decode("utf-8", errors="replace")
    if "manifest unknown" in error or "name unknown" in error:
        return None
    raise ArtifactError("could not establish registry manifest state")


def identity(archive, ctx):
    source = "oci-archive:" + str(archive)
    raw = command(["skopeo", "inspect", "--raw", source], capture=True)
    manifest = json.loads(raw)
    config_raw = command(["skopeo", "inspect", "--raw", "--config", source], capture=True)
    config = json.loads(config_raw)
    if config.get("architecture") != "amd64" or config.get("os") != "linux":
        raise ArtifactError("artifact must be linux/amd64")
    if config.get("config", {}).get("Labels", {}).get("org.opencontainers.image.revision") != ctx["GITHUB_SHA"]:
        raise ArtifactError("artifact source revision mismatch")
    config_digest = manifest.get("config", {}).get("digest")
    if digest(config_raw) != config_digest:
        raise ArtifactError("artifact configuration digest mismatch")
    return {"manifest_digest": digest(raw), "config_digest": config_digest,
            "archive_digest": file_digest(archive)}


def prepare(directory):
    ctx = context()
    version_tag(ctx)
    archive = directory / "image.oci.tar"
    directory.mkdir(parents=True, exist_ok=False)
    revision = IMAGE + ":sha-" + ctx["GITHUB_SHA"]
    existing = remote_digest(revision) if trusted(ctx) else None
    if existing:
        # A rerun or version-tag run validates the already immutable revision.
        command(["skopeo", "copy", "--preserve-digests", "docker://" + IMAGE + "@" + existing,
                 "oci-archive:" + str(archive)])
    else:
        command(["docker", "buildx", "create", "--name", "artifact-builder", "--driver", "docker-container", "--use"])
        command(["docker", "buildx", "build", "--platform", "linux/amd64", "--provenance=false", "--sbom=false",
                 "--label", "org.opencontainers.image.revision=" + ctx["GITHUB_SHA"],
                 "--output", "type=oci,dest=" + str(archive), "."])
    evidence = identity(archive, ctx)
    if existing and evidence["manifest_digest"] != existing:
        raise ArtifactError("existing revision changed while preparing artifact")
    local_tag = "mcp-gitea-rs:validated-candidate"
    command(["skopeo", "copy", "oci-archive:" + str(archive), "docker-daemon:" + local_tag])
    loaded = command(["docker", "image", "inspect", "--format", "{{.Id}}", local_tag], capture=True).decode().strip()
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", loaded):
        raise ArtifactError("invalid loaded image identity")
    # Docker's containerd store identifies an image by manifest; the classic
    # store identifies it by configuration. Compare raw configuration bytes in
    # either case and run tests using the daemon's immutable image identity.
    loaded_config = command(["skopeo", "inspect", "--raw", "--config", "docker-daemon:" + loaded], capture=True)
    if digest(loaded_config) != evidence["config_digest"]:
        raise ArtifactError("loaded test image differs from artifact")
    (directory / "candidate.json").write_text(json.dumps({"context": ctx, **evidence}, sort_keys=True))
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        output.write("image=" + loaded + "\n")


def record(directory):
    ctx = context()
    candidate = json.loads((directory / "candidate.json").read_text())
    evidence = {"context": ctx, **identity(directory / "image.oci.tar", ctx)}
    if candidate != evidence:
        raise ArtifactError("artifact changed during validation")
    evidence["checks"] = CHECKS
    (directory / "validated.json").write_text(json.dumps(evidence, sort_keys=True))
    print("Validated manifest: " + evidence["manifest_digest"])


def verify(directory):
    ctx = context()
    if not trusted(ctx):
        raise ArtifactError("publication requires a canonical main or version-tag push")
    tag = version_tag(ctx)
    evidence = json.loads((directory / "validated.json").read_text())
    expected = {"context": ctx, **identity(directory / "image.oci.tar", ctx), "checks": CHECKS}
    if evidence != expected:
        raise ArtifactError("missing, stale, or mismatched validation evidence")
    return ctx, tag, expected


def publish(directory):
    ctx, tag, evidence = verify(directory)
    expected = evidence["manifest_digest"]
    revision = IMAGE + ":sha-" + ctx["GITHUB_SHA"]
    immutable = [revision] + ([IMAGE + ":" + tag] if tag else [])
    existing = {reference: remote_digest(reference) for reference in immutable}
    if any(value is not None and value != expected for value in existing.values()):
        raise ArtifactError("immutable tag already names different image contents")
    targets = immutable + ([IMAGE + ":latest"] if ctx["GITHUB_REF"] == "refs/heads/main" else [])
    for reference in targets:
        if existing.get(reference) != expected:
            command(["skopeo", "copy", "--preserve-digests", "oci-archive:" + str(directory / "image.oci.tar"),
                     "docker://" + reference])
        if remote_digest(reference) != expected:
            raise ArtifactError("published manifest digest differs from validated artifact")
        print(reference + " = " + expected)
    with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as output:
        output.write("Validated and published image: `" + IMAGE + "@" + expected + "`\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["prepare", "record", "verify", "publish"])
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    try:
        {"prepare": prepare, "record": record, "verify": verify, "publish": publish}[args.action](args.directory.resolve())
    except (ArtifactError, ValueError, KeyError, OSError, subprocess.TimeoutExpired) as error:
        raise SystemExit(str(error)) from None


if __name__ == "__main__":
    main()
