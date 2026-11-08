import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
GENERATOR = Path(os.environ.get("ZERQON_GENERATOR_UNDER_TEST", ROOT / "scripts/generate-test-world.py"))

HELPER = r'''
import json
import os
from pathlib import Path
import sys
import time

root, mode = Path(sys.argv[1]), sys.argv[2]
(root / "child.pid").write_text(str(os.getpid()), encoding="utf-8")
print("Done (0.1s)! For help, type help", flush=True)
if mode == "stalled":
    time.sleep(60)
else:
    with (root / "console.log").open("w", encoding="utf-8") as log:
        for line in sys.stdin:
            log.write(line)
            log.flush()
            if "run say ZERQON_BENCH_READY_" in line:
                print("[Server] " + line.split("run say ", 1)[1].strip(), flush=True)
            if line.strip() == "stop":
                region = Path.cwd() / "world/region"
                region.mkdir(parents=True)
                (region / "r.0.0.mca").write_bytes(b"script-control fixture, not an Anvil world")
                break
'''

RUNNER = r'''
import json
from pathlib import Path
import runpy
import subprocess
import sys
import threading

generator, root, mode = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
real_popen = subprocess.Popen

def java_probe(command, **kwargs):
    if command != ["java", "-XshowSettings:properties", "-version"]:
        raise AssertionError(command)
    return subprocess.CompletedProcess(command, 0, "", "    java.home = " + str(root / "jdk") + "\n")

class JavaProcess(real_popen):
    def __init__(self, command, **kwargs):
        (root / "java-command.json").write_text(json.dumps(command), encoding="utf-8")
        if mode == "stalled" and sys.platform == "linux":
            kwargs["pipesize"] = 4096
        super().__init__([sys.executable, str(root / "helper.py"), str(root), mode], **kwargs)

    def wait(self, *args, **kwargs):
        code = super().wait(*args, **kwargs)
        (root / "reaped.json").write_text(json.dumps({"pid": self.pid, "code": code}), encoding="utf-8")
        return code

subprocess.run = java_probe
subprocess.Popen = JavaProcess
if mode == "writer_start_failure":
    real_start = threading.Thread.start
    def start_thread(thread):
        if thread.name == "zerqon-fixture-stdin":
            raise OSError("controlled writer startup failure")
        return real_start(thread)
    threading.Thread.start = start_thread
sys.argv = [str(generator), *sys.argv[4:]]
runpy.run_path(str(generator), run_name="__main__")
'''


class GeneratorTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="zerqon-generator-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / "helper.py").write_text(HELPER, encoding="utf-8")
        (self.root / "runner.py").write_text(RUNNER, encoding="utf-8")
        self.jar = self.root / "fixture.jar"
        self.jar.write_bytes(b"launcher boundary fixture")
        self.world = self.root / "generated"

    def run_generator(self, mode="responsive", terrain="normal", side=2):
        command = [
            sys.executable, str(self.root / "runner.py"), str(GENERATOR), str(self.root), mode,
            "--server-jar", str(self.jar), "--directory", str(self.world), "--accept-eula",
            "--terrain", terrain, "--side-chunks", str(side), "--timeout-seconds", "1",
        ]
        if terrain == "normal":
            command.extend(["--seed", "20261108"])
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8")
        try:
            stdout, stderr = process.communicate(timeout=8)
        except subprocess.TimeoutExpired:
            pid_file = self.root / "child.pid"
            if pid_file.exists():
                try:
                    os.kill(int(pid_file.read_text(encoding="utf-8")), 9)
                except OSError:
                    pass
            try:
                process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate(timeout=5)
            self.fail("generator watchdog could not interrupt blocked console writing")
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate(timeout=5)
        return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)

    def assert_reaped(self, success):
        child = int((self.root / "child.pid").read_text(encoding="utf-8"))
        result = json.loads((self.root / "reaped.json").read_text(encoding="utf-8"))
        self.assertEqual(result["pid"], child)
        self.assertEqual(result["code"] == 0, success)

    def test_stalled_console_times_out_and_reaps_only_its_owned_process(self):
        result = self.run_generator(mode="stalled", side=16)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("console write", result.stderr)
        self.assertIn("TimeoutError", result.stderr)
        self.assert_reaped(success=False)
        self.assertFalse((self.world / "fixture.json").exists())

    def test_normal_fixture_waits_for_each_requested_chunk_and_stops_cleanly(self):
        result = self.run_generator()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_reaped(success=True)
        console = (self.root / "console.log").read_text(encoding="utf-8")
        self.assertIn("forceload add 0 0 31 31\n", console)
        self.assertEqual(console.count("execute if loaded "), 4)
        self.assertTrue(console.endswith("save-all flush\nstop\n"))
        self.assertNotIn("setblock", console)
        properties = (self.world / "server.properties").read_text(encoding="utf-8")
        self.assertIn("level-type=minecraft:normal\n", properties)
        self.assertIn("level-seed=20261108\n", properties)
        fixture = json.loads((self.world / "fixture.json").read_text(encoding="utf-8"))
        self.assertEqual(fixture["requested_chunk_rectangle"], [0, 0, 1, 1])
        self.assertEqual(fixture["seed"], 20261108)

    def test_flat_fixture_keeps_its_two_marked_chunks(self):
        result = self.run_generator(terrain="flat")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_reaped(success=True)
        console = (self.root / "console.log").read_text(encoding="utf-8")
        self.assertIn("forceload add -1 -513 -1 -513\n", console)
        self.assertIn("setblock 0 70 0 minecraft:diamond_block\n", console)
        self.assertIn("setblock -1 70 -513 minecraft:oak_log[axis=x]\n", console)
        self.assertTrue(console.endswith("save-all flush\nstop\n"))
        self.assertNotIn("ZERQON_BENCH_READY", console)

    def test_existing_directory_is_preserved_before_launch(self):
        self.world.mkdir()
        retained = self.world / "keep.txt"
        retained.write_text("existing fixture", encoding="utf-8")
        result = self.run_generator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("existing worlds are never overwritten", result.stderr)
        self.assertEqual(retained.read_text(encoding="utf-8"), "existing fixture")
        self.assertFalse((self.root / "child.pid").exists())

    def test_writer_startup_failure_still_reaps_the_spawned_process(self):
        result = self.run_generator(mode="writer_start_failure")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("controlled writer startup failure", result.stderr)
        reaped = json.loads((self.root / "reaped.json").read_text(encoding="utf-8"))
        self.assertNotEqual(reaped["code"], 0)
        self.assertFalse((self.world / "fixture.json").exists())


if __name__ == "__main__":
    unittest.main()
