import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("prepare_data", Path(__file__).with_name("prepare-data.py"))
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class DataPreparationTests(unittest.TestCase):
    def test_nested_tags_expand_in_order_without_duplicate_ids(self):
        definitions = {"minecraft:base": ["minecraft:stone"], "minecraft:all": ["#minecraft:base", "minecraft:dirt", "minecraft:stone", {"id": "minecraft:missing", "required": False}]}
        self.assertEqual(prepare.resolve_tags(definitions, {"minecraft:stone": 1, "minecraft:dirt": 2}), {"minecraft:base": [1], "minecraft:all": [1, 2]})

    def test_required_missing_tag_members_are_rejected(self):
        with self.assertRaises(ValueError):
            prepare.resolve_tags({"minecraft:all": ["minecraft:missing"]}, {})

    def test_tag_cycles_are_rejected(self):
        with self.assertRaises(ValueError):
            prepare.resolve_tags({"minecraft:a": ["#minecraft:b"], "minecraft:b": ["#minecraft:a"]}, {})

    def test_cached_download_requires_matching_content(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "server.jar"
            path.write_bytes(b"abc")
            self.assertTrue(prepare.verified_file(path, "a9993e364706816aba3e25717850c26c9cd0d89d"))
            path.write_bytes(b"corrupt")
            self.assertFalse(prepare.verified_file(path, "a9993e364706816aba3e25717850c26c9cd0d89d"))

    def test_registry_entries_preserve_file_identifier_order_and_json(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for namespace, name in [("minecraft", "x"), ("minecraft", "x-y"), ("other", "x")]:
                path = root / namespace / "dimension_type" / f"{name}.json"
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps({"height": 384, "has_skylight": True}), encoding="utf-8")
            entries = prepare.registry_entries(root, "minecraft:dimension_type")
            self.assertEqual([entry["id"] for entry in entries], ["minecraft:x-y", "minecraft:x", "other:x"])
            self.assertEqual(entries[0]["data"], {"height": 384, "has_skylight": True})


if __name__ == "__main__":
    unittest.main()
