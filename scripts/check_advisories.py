"""Check locked crates.io versions against OSV; incomplete scans fail closed."""
import json
from pathlib import Path
import sys
import tomllib
import urllib.error
import urllib.request

OSV_URL = "https://api.osv.dev/v1/querybatch"
PUBLIC_REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def queries_from_lock(lock):
    return [{"package": {"name": package["name"], "ecosystem": "crates.io"},
             "version": package["version"]}
            for package in lock["package"] if package.get("source") == PUBLIC_REGISTRY]


def request_batch(queries):
    request = urllib.request.Request(OSV_URL, data=json.dumps({"queries": queries}).encode(),
                                     headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=45) as response:
        return json.load(response)


def check(queries, request=request_batch):
    findings = set()
    for start in range(0, len(queries), 100):
        pending = queries[start:start + 100]
        for _ in range(20):
            results = request(pending)["results"]
            if len(results) != len(pending):
                raise ValueError("OSV response count does not match the queries")
            next_page = []
            for query, result in zip(pending, results):
                for vulnerability in result.get("vulns", []):
                    findings.add((query["package"]["name"], query["version"], vulnerability["id"]))
                if result.get("next_page_token"):
                    next_page.append(query | {"page_token": result["next_page_token"]})
            if not next_page:
                break
            pending = next_page
        else:
            raise ValueError("OSV pagination limit reached; scan incomplete")
    return sorted(findings)


def main():
    root = Path(__file__).resolve().parent.parent
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    unsupported = [package["name"] for package in lock["package"]
                   if package.get("source") and package["source"] != PUBLIC_REGISTRY]
    if unsupported:
        raise SystemExit("Non-crates.io dependencies need a separate advisory check")
    queries = queries_from_lock(lock)
    if not queries:
        raise SystemExit("No public registry dependencies found; scan incomplete")
    try:
        findings = check(queries)
    except (urllib.error.URLError, TimeoutError, ValueError, KeyError, TypeError):
        raise SystemExit("OSV check failed or returned incomplete data; no clean result recorded") from None
    for name, version, identifier in findings:
        print(f"{name} {version}: {identifier}")
    print(f"OSV checked {len(queries)} locked packages; {len(findings)} advisory matches")
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
