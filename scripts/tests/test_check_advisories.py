import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("check_advisories", ROOT / "scripts/check_advisories.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class AdvisoryTests(unittest.TestCase):
    def test_only_public_registry_packages_are_queried(self):
        lock = {"package": [{"name": "public", "version": "1.0.0", "source": module.PUBLIC_REGISTRY},
                            {"name": "private", "version": "1.0.0", "source": "registry+https://private.invalid"},
                            {"name": "workspace", "version": "1.0.0"}]}
        self.assertEqual(module.queries_from_lock(lock), [
            {"package": {"name": "public", "ecosystem": "crates.io"}, "version": "1.0.0"}])

    def test_paginated_findings_are_not_lost(self):
        query = {"package": {"name": "crate", "ecosystem": "crates.io"}, "version": "1.0.0"}
        calls = []
        def request(queries):
            calls.append(queries)
            if len(calls) == 1:
                return {"results": [{"next_page_token": "next"}]}
            return {"results": [{"vulns": [{"id": "RUSTSEC-example"}]}]}
        self.assertEqual(module.check([query], request), [("crate", "1.0.0", "RUSTSEC-example")])
        self.assertEqual(calls[1], [query | {"page_token": "next"}])

    def test_incomplete_results_fail(self):
        with self.assertRaises(ValueError):
            module.check([{}], lambda _: {"results": []})
        with self.assertRaises(ValueError):
            module.check([{}], lambda _: {"results": [{"next_page_token": "forever"}]})

    def test_clean_results_are_successful(self):
        self.assertEqual(module.check([{}], lambda _: {"results": [{}]}), [])
