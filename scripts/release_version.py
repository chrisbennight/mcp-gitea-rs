"""Validate stable release tags against the workspace version."""
import os
from pathlib import Path
import re
import tomllib


def release_tag(ref, version):
    if not ref.startswith("refs/tags/"):
        return None
    tag = ref.removeprefix("refs/tags/")
    if not re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
        raise ValueError("release tags must use vMAJOR.MINOR.PATCH without leading zeroes")
    if tag != "v" + version:
        raise ValueError("release tag must match the Cargo workspace version")
    return tag


def main():
    root = Path(__file__).resolve().parent.parent
    version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    try:
        tag = release_tag(os.environ.get("GITHUB_REF", ""), version)
    except ValueError as error:
        raise SystemExit(str(error)) from None
    if tag:
        print(tag)


if __name__ == "__main__":
    main()
