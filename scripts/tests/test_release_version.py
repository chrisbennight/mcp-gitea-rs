import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("release_version", ROOT / "scripts/release_version.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseVersionTests(unittest.TestCase):
    def test_stable_tag_must_match_version(self):
        self.assertEqual(release.release_tag("refs/tags/v0.1.0", "0.1.0"), "v0.1.0")
        self.assertIsNone(release.release_tag("refs/heads/main", "0.1.0"))
        for ref in ["refs/tags/v0.2.0", "refs/tags/v01.1.0", "refs/tags/v0.1.0-rc.1",
                    "refs/tags/v0.1.0/other", "refs/tags/v0.1.0\n"]:
            with self.subTest(ref=ref), self.assertRaises(ValueError):
                release.release_tag(ref, "0.1.0")
