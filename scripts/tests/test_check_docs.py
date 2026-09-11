import subprocess
import sys
import unittest
from pathlib import Path


class DocumentationValidatorTests(unittest.TestCase):
    def test_repository_documentation_has_no_broken_local_links(self):
        root = Path(__file__).resolve().parents[2]
        result = subprocess.run(
            [sys.executable, "scripts/check_docs.py"],
            cwd=root,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
