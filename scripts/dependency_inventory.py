"""Write distribution metadata without leaking local Cargo paths or source credentials."""
import hashlib
import json
from pathlib import Path
import sys


def inventory(metadata, lock_bytes):
    packages = [
        {"name": package["name"], "version": package["version"],
         "license": package.get("license"),
         "workspace": package["id"] in metadata["workspace_members"]}
        for package in metadata["packages"]
    ]
    packages.sort(key=lambda package: (package["name"], package["version"]))
    return {"cargo_lock_sha256": hashlib.sha256(lock_bytes).hexdigest(),
            "scope": "Cargo resolution, including platform and development dependencies",
            "packages": packages}


def main():
    metadata_path, lock_path, destination = map(Path, sys.argv[1:])
    result = inventory(json.loads(metadata_path.read_text()), lock_path.read_bytes())
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
