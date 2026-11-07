import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("test_client", Path(__file__).with_name("test-client.py"))
client = importlib.util.module_from_spec(spec)
spec.loader.exec_module(client)


class ClientPreparationTests(unittest.TestCase):
    def test_rules_select_host_and_last_matching_action(self):
        self.assertTrue(client.allowed([]))
        self.assertFalse(client.allowed([{"action": "allow", "os": {"name": "linux"}}]))
        self.assertFalse(client.allowed([{"action": "allow"}, {"action": "disallow", "os": {"name": "windows"}}]))
        self.assertTrue(client.allowed([{"action": "allow", "features": {"is_quick_play_multiplayer": True}}], features={"is_quick_play_multiplayer": True}))
        self.assertFalse(client.allowed([{"action": "allow", "features": {"is_demo_user": True}}]))

    def test_library_paths_cannot_escape_cache(self):
        with tempfile.TemporaryDirectory() as directory:
            for path in ["../outside.jar", "C:/outside.jar", "/outside.jar", "a\\outside.jar"]:
                with self.assertRaises(ValueError):
                    client.safe_artifact_path(Path(directory), path)
            self.assertEqual(client.safe_artifact_path(Path(directory), "a/b.jar"), Path(directory) / "a/b.jar")

    def test_offline_identity_matches_minecraft(self):
        self.assertEqual(client.offline_uuid("Notch"), "b50ad385829d3141a2167e7d7539ba7f")

    def test_verified_local_copy_is_used_without_network(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.jar"
            target = root / "target.jar"
            source.write_bytes(b"abc")
            target.write_bytes(b"corrupt")
            client.ensure_artifact(target, "a9993e364706816aba3e25717850c26c9cd0d89d", "https://invalid.invalid/never", source)
            self.assertEqual(target.read_bytes(), b"abc")

    def test_bad_download_does_not_replace_an_existing_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "bad.jar"
            target = root / "target.jar"
            source.write_bytes(b"bad")
            target.write_bytes(b"preserved")
            with self.assertRaises(ValueError):
                client.ensure_artifact(target, "a9993e364706816aba3e25717850c26c9cd0d89d", source.as_uri())
            self.assertEqual(target.read_bytes(), b"preserved")


if __name__ == "__main__":
    unittest.main()
