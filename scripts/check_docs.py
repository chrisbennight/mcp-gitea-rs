from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
LINK = re.compile(r"\[[^\]]+\]\(([^)]+)\)")


def main() -> int:
    failures = []
    for document in ROOT.rglob("*.md"):
        if ".git" in document.parts or "target" in document.parts:
            continue
        for target in LINK.findall(document.read_text()):
            if "://" in target or target.startswith("#"):
                continue
            path = target.split("#", 1)[0]
            if path and not (document.parent / path).exists():
                failures.append(f"{document.relative_to(ROOT)}: {target}")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
