"""Behavioral checks for the official crafting exporter and recipe oracle."""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
JAVA = "java"
JAVAC = "javac"


class CraftingDataTests(unittest.TestCase):
    def setUp(self):
        (ROOT / ".cache/crafting-data").mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(prefix="test-", dir=ROOT / ".cache/crafting-data")
        self.addCleanup(self.temporary.cleanup)
        self.stage = Path(self.temporary.name)

    def run_export(self, *arguments):
        return subprocess.run(
            [sys.executable, str(HERE / "generate.py"), "--java", JAVA, "--javac", JAVAC,
             "--output", str(self.stage / "facts.json"), *map(str, arguments)],
            cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=120,
        )

    def fixture(self, expected):
        path = self.stage / "fixture.json"
        path.write_text(json.dumps({"schema": 1, "minecraft": "26.3", "protocol": 777, "cases": [{
            "name": "honey", "recipe": "minecraft:honey_block",
            "input": ["minecraft:honey_bottle"] * 4, "expected": expected,
        }]}), encoding="utf-8")
        return path

    @staticmethod
    def honey_expected():
        return {"matches": True, "result": {"item": "minecraft:honey_block", "count": 1, "components": {}},
                "remainders": [{"item": "minecraft:glass_bottle", "count": 1, "components": {}}] * 4}

    def test_export_uses_crafting_remainder_and_is_reproducible(self):
        result = self.run_export()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        path = self.stage / "facts.json"
        first = path.read_bytes()
        facts = json.loads(first)
        remainders = {row[0]: row[1:] for row in facts["remainders"]}
        self.assertEqual(remainders["minecraft:honey_bottle"], ["minecraft:glass_bottle", 1])
        self.assertEqual(remainders["minecraft:milk_bucket"], ["minecraft:bucket", 1])
        self.assertNotIn("minecraft:stone", remainders)
        self.assertEqual(facts["item_count"], 1658)
        self.assertEqual(facts["data_version"], 5023)
        oracle = json.loads((self.stage / "oracle.json").read_text())
        self.assertEqual(oracle["tags"]["minecraft:oak_logs"], ["minecraft:oak_log", "minecraft:oak_wood", "minecraft:stripped_oak_log", "minecraft:stripped_oak_wood"])
        self.assertEqual(oracle["recipes"]["minecraft:torch"]["key"]["X"], ["minecraft:coal", "minecraft:charcoal"])
        self.assertEqual(oracle["items"]["minecraft:honey_bottle"]["max_stack"], 16)
        self.assertIsNone(oracle["items"]["minecraft:honey_bottle"]["max_damage"])
        self.assertEqual(oracle["items"]["minecraft:mushroom_stew"]["max_stack"], 1)
        self.assertIsInstance(oracle["components"]["minecraft:custom_name"], int)
        checked = self.run_export("--check")
        self.assertEqual(checked.returncode, 0, checked.stdout + checked.stderr)
        self.assertEqual(path.read_bytes(), first)

    def test_incorrect_expected_result_is_detected(self):
        expected = self.honey_expected()
        expected["result"]["count"] = 2
        result = self.run_export("--fixtures", self.fixture(expected))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Recipe observation differs", result.stdout + result.stderr)
        self.assertFalse((self.stage / "facts.json").exists())

    def test_missing_crafting_remainder_is_detected(self):
        expected = self.honey_expected()
        expected["remainders"] = [None] * 4
        result = self.run_export("--fixtures", self.fixture(expected))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Recipe observation differs", result.stdout + result.stderr)

    def test_unknown_items_fail_closed(self):
        path = self.fixture(self.honey_expected())
        value = json.loads(path.read_text())
        value["cases"][0]["input"][0] = "minecraft:misspelled_honey_bottle"
        path.write_text(json.dumps(value), encoding="utf-8")
        result = self.run_export("--fixtures", path)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unknown input item", result.stdout + result.stderr)

    def test_oversized_fixture_is_rejected_before_java(self):
        path = self.stage / "large.json"
        path.write_bytes(b" " * (1_048_576 + 1))
        result = self.run_export("--fixtures", path, "--java", "absent-java-must-not-run")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Fixture exceeds 1 MiB", result.stdout + result.stderr)

    def test_modified_bundle_is_rejected_before_java(self):
        data = self.stage / "data"
        data.mkdir()
        (data / "server.jar").write_bytes(b"modified bundle")
        result = self.run_export("--data", data, "--java", "absent-java-must-not-run")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Official server.jar checksum mismatch", result.stdout + result.stderr)

    def test_check_rejects_modified_asset(self):
        path = self.stage / "facts.json"
        incorrect = b'{"schema":1,"remainders":[]}\n'
        path.write_bytes(incorrect)
        result = self.run_export("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Crafting facts differ from the official server", result.stdout + result.stderr)

    def test_check_rejects_modified_oracle(self):
        (self.stage / "facts.json").write_bytes((HERE / "26.3.json").read_bytes())
        (self.stage / "oracle.json").write_bytes(b"{}\n")
        result = self.run_export("--check")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Crafting oracle export differs from the official server", result.stdout + result.stderr)

    def test_output_cannot_replace_input_fixtures(self):
        path = self.fixture(self.honey_expected())
        original = path.read_bytes()
        result = self.run_export("--fixtures", path, "--output", path, "--java", "absent-java-must-not-run")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must not overwrite input fixtures", result.stdout + result.stderr)
        self.assertEqual(path.read_bytes(), original)

    def codec_fixture(self, overrides, expected):
        path = self.fixture(self.honey_expected())
        value = json.loads(path.read_text())
        value["codec_cases"] = [{"name": "custom_torch", "base_recipe": "minecraft:torch",
                                 "overrides": overrides, "input": ["minecraft:coal", None, "minecraft:stick", None],
                                 "expected": expected}]
        path.write_text(json.dumps(value), encoding="utf-8")
        return path

    @staticmethod
    def torch_codec_expected():
        return {"accepted": True, "show_notification": True, "matches": True,
                "result": {"item": "minecraft:torch", "count": 1, "components": {}}, "remainders": [None] * 4}

    def test_custom_codec_variants_export_actual_results(self):
        path = self.codec_fixture({"result": "minecraft:torch"}, self.torch_codec_expected())
        result = self.run_export("--fixtures", path)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        oracle = json.loads((self.stage / "oracle.json").read_text())
        self.assertIn("codec_cases", oracle)
        case = oracle["codec_cases"][0]
        self.assertEqual(case["source"], "custom_codec_variant")
        self.assertEqual(case["recipe"]["result"], "minecraft:torch")
        self.assertEqual(case["expected"], self.torch_codec_expected())
        self.assertEqual(case["input"][0], {"item": "minecraft:coal", "count": 1, "components": {}})

    def test_incorrect_custom_codec_acceptance_is_detected(self):
        path = self.codec_fixture({"group": {}}, self.torch_codec_expected())
        result = self.run_export("--fixtures", path)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Codec observation differs", result.stdout + result.stderr)
        self.assertFalse((self.stage / "facts.json").exists())

    def test_missing_codec_base_is_not_an_expected_codec_rejection(self):
        path = self.codec_fixture({"group": {}}, {"accepted": False})
        value = json.loads(path.read_text())
        value["codec_cases"][0]["base_recipe"] = "minecraft:missing_recipe"
        path.write_text(json.dumps(value), encoding="utf-8")
        result = self.run_export("--fixtures", path)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Missing original JSON", result.stdout + result.stderr)
        self.assertFalse((self.stage / "facts.json").exists())

    def test_strict_rejections_preserve_original_acceptance(self):
        path = self.codec_fixture({"result": {"id": "minecraft:torch", "count": 1.5}}, self.torch_codec_expected())
        value = json.loads(path.read_text())
        value["strict_rejections"] = value.pop("codec_cases")
        value["strict_rejections"][0]["rust_reject"] = True
        path.write_text(json.dumps(value), encoding="utf-8")
        result = self.run_export("--fixtures", path)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        oracle = json.loads((self.stage / "oracle.json").read_text())
        self.assertIn("strict_rejections", oracle)
        case = oracle["strict_rejections"][0]
        self.assertTrue(case["rust_reject"])
        self.assertEqual(case["recipe"]["result"]["count"], 1.5)
        self.assertEqual(case["expected"], self.torch_codec_expected())
        self.assertEqual(oracle["codec_cases"], [])

    def test_strict_rejection_requires_real_original_acceptance(self):
        path = self.codec_fixture({"group": {}}, {"accepted": False})
        value = json.loads(path.read_text())
        value["strict_rejections"] = value.pop("codec_cases")
        value["strict_rejections"][0]["rust_reject"] = True
        path.write_text(json.dumps(value), encoding="utf-8")
        result = self.run_export("--fixtures", path)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Strict rejection fixtures must be accepted by the original codec", result.stdout + result.stderr)

    def test_free_form_group_text_is_not_interpreted_as_an_item_identifier(self):
        expected = self.torch_codec_expected()
        expected["result"]["count"] = 4
        path = self.codec_fixture({"group": "minecraft:display group"}, expected)
        result = self.run_export("--fixtures", path)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        oracle = json.loads((self.stage / "oracle.json").read_text())
        self.assertEqual(oracle["codec_cases"][0]["recipe"]["group"], "minecraft:display group")
        self.assertEqual(oracle["codec_cases"][0]["expected"], expected)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--java", default="java")
    parser.add_argument("--javac", default="javac")
    options, remaining = parser.parse_known_args()
    JAVA, JAVAC = options.java, options.javac
    unittest.main(argv=[__file__, *remaining])
