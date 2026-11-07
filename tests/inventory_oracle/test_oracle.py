import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
JAVA = "java"
JAVAC = "javac"


class OracleTests(unittest.TestCase):
    def run_fixture(self, fixture, *options):
        with tempfile.TemporaryDirectory(prefix="zerqon-inventory-oracle-") as temporary:
            path = Path(temporary) / "fixture.json"
            path.write_text(json.dumps({"version": "26.3", "protocol": 777, "fixtures": [fixture]}), encoding="utf-8")
            return subprocess.run(
                [sys.executable, str(ROOT / "scripts/test-inventory-codec.py"), "--java", JAVA, "--javac", JAVAC, "--fixtures", str(path), *options],
                cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=120,
            )

    @staticmethod
    def empty_fixture():
        return {"name": "empty", "kind": "creative_slot", "body_hex": "002400", "expected": {"slot": 36, "stack": None}}

    def test_incorrect_expected_slot_is_detected(self):
        fixture = self.empty_fixture()
        fixture["expected"]["slot"] = 37
        result = self.run_fixture(fixture)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Decoded value differs", result.stdout + result.stderr)

    def test_trailing_packet_bytes_are_detected(self):
        fixture = self.empty_fixture()
        fixture["body_hex"] += "00"
        result = self.run_fixture(fixture)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Trailing packet bytes", result.stdout + result.stderr)

    def test_unknown_codec_cannot_satisfy_an_expected_decode_error(self):
        fixture = self.empty_fixture()
        fixture["kind"] = "misspelled_codec"
        fixture["expect_error"] = True
        result = self.run_fixture(fixture)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unknown fixture kind", result.stdout + result.stderr)

    def test_validated_export_preserves_empty_slots_and_default_item_domain(self):
        with tempfile.TemporaryDirectory(prefix="zerqon-inventory-export-") as directory:
            output = Path(directory) / "official.json"
            result = self.run_fixture(self.empty_fixture(), "--export", str(output), "--export-default-items")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            exported = json.loads(output.read_text(encoding="utf-8"))
            fixtures = {fixture["name"]: fixture for fixture in exported["fixtures"]}
            self.assertEqual(fixtures["creative_empty"]["expected"], {"slot": 36, "stack": None})
            self.assertIsNone(fixtures["server_empty"]["expected"])
            self.assertEqual(sum(name.startswith("default/") for name in fixtures), 1658)
            self.assertEqual(fixtures["default/minecraft:stone"]["expected"]["item"], "minecraft:stone")
            self.assertNotEqual(fixtures["creative_custom_name"]["body_hex"][4:], fixtures["server_custom_name"]["body_hex"])


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--java", default="java")
    parser.add_argument("--javac", default="javac")
    args, remaining = parser.parse_known_args()
    JAVA, JAVAC = args.java, args.javac
    unittest.main(argv=[__file__, *remaining])
