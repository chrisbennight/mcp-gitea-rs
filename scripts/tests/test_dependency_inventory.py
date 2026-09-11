import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("dependency_inventory", ROOT / "scripts/dependency_inventory.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class InventoryTests(unittest.TestCase):
    def test_inventory_excludes_machine_paths_and_registry_credentials(self):
        metadata = {"workspace_members": ["local"], "packages": [
            {"id": "remote", "name": "dependency", "version": "1.0.0", "license": "MIT",
             "source": "https://user:private-registry-password@example.test", "manifest_path": "/private/cache/Cargo.toml"},
            {"id": "local", "name": "application", "version": "0.1.0", "license": "MIT"},
        ]}
        result = module.inventory(metadata, b"lock")
        self.assertEqual(result["packages"], [
            {"name": "application", "version": "0.1.0", "license": "MIT", "workspace": True},
            {"name": "dependency", "version": "1.0.0", "license": "MIT", "workspace": False},
        ])
        self.assertNotIn("private", str(result))
