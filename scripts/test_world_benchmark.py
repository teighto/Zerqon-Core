"""Real Linux child-process checks for the world benchmark driver."""

import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import platform
import signal
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "world_benchmark", Path(__file__).with_name("benchmark-world-load.py")
)
DRIVER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DRIVER)


CHILD = r'''
import hashlib
import json
import os
from pathlib import Path
import sys
import time

world, selection = map(Path, sys.argv[1:3])
workers, passes = map(int, sys.argv[3:5])
assert sys.argv[5:] == ["--coordinated"]
config = json.loads((world / "fixture.json").read_text())
mode = config.get("mode", "valid")
with (world / "pids").open("a") as file:
    file.write(str(os.getpid()) + "\n")

def trace(value):
    with (world / "trace").open("a") as file:
        file.write(value + "\n")

def emit(value):
    trace(value["event"])
    print(json.dumps(value), flush=True)

def receive(expected):
    actual = sys.stdin.readline().strip()
    trace(actual)
    assert actual == expected, (actual, expected)

def burn(seconds):
    start = time.process_time()
    while time.process_time() - start < seconds:
        sum(range(500))

def selection_hash():
    return hashlib.sha256(selection.read_bytes()).hexdigest()

common = {
    "mode": "warm_imported_block_read_decode", "workers": workers,
    "passes": passes, "selected_chunks": len(json.loads(selection.read_text())["chunks"]),
    "provenance": {"selection_sha256": selection_hash()},
}
ready = dict(common, event="ready", queue_capacity=workers * 2, inflight_limit=workers)
ready.update(config.get("ready", {}))
if mode == "wrong_phase":
    emit(dict(ready, event="measured"))
    time.sleep(30)
if mode == "flood":
    for _ in range(1000):
        emit(dict(ready, event="unexpected"))
    time.sleep(30)
if mode == "invalid_json":
    print("not json", flush=True)
    time.sleep(30)
if mode == "non_object":
    print("[]", flush=True)
    time.sleep(30)
if mode == "oversized":
    print("x" * 131073, flush=True)
    time.sleep(30)
emit(ready)
receive("go")
if mode == "timeout":
    time.sleep(30)
if mode == "early_exit":
    sys.exit(7)
if mode == "mutate_selection":
    selection.write_text(selection.read_text() + " ")
    common["provenance"] = {"selection_sha256": selection_hash()}
start = time.monotonic()
burn(0.04 if mode == "usage" else 0.002)
wall = time.monotonic() - start
measured = dict(common, event="measured", completed_loads=common["selected_chunks"] * passes,
                timed_loop_wall_seconds=wall, chunks_per_second=common["selected_chunks"] * passes / wall)
measured.update(config.get("measured", {}))
emit(measured)
receive("verify")
verified = {"event": "verified", "all_selected_chunks_match": True,
            "selection_sha256": selection_hash()}
verified.update(config.get("verified", {}))
emit(verified)
receive("exit")
if mode == "teardown_timeout":
    time.sleep(30)
if mode == "usage":
    allocation = bytearray(32 * 1024 * 1024)
    burn(0.10)
if mode == "nonzero":
    sys.exit(9)
'''


@unittest.skipUnless(platform.system() == "Linux" and hasattr(os, "wait4"), "requires Linux /proc and wait4")
class WorldBenchmarkTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="zerqon-benchmark-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.world = self.root / "world"
        self.world.mkdir()
        (self.world / "chunks.redb").write_bytes(b"fixture")
        self.binary = self.root / "child"
        self.binary.write_text("#!" + sys.executable + "\n" + textwrap.dedent(CHILD), encoding="utf-8")
        self.binary.chmod(0o755)
        self.selection = self.root / "selection.json"
        self.selection.write_text(json.dumps({"chunks": [{"x": 0, "z": 0}, {"x": 1, "z": 0}]}), encoding="utf-8")
        self.settings = self.root / "server.settings"
        self.settings.write_text("world_threads = 4\n", encoding="utf-8")
        self.output = self.root / "result.json"
        self.configure()

    def configure(self, **config):
        (self.world / "fixture.json").write_text(json.dumps(config), encoding="utf-8")

    def run_child(self, output=None, workers=1, passes=1, timeout=2):
        return DRIVER.one_run(self.binary, self.world, self.selection, workers, passes,
                              output or self.output, timeout)

    def assert_children_reaped(self):
        for pid in (self.world / "pids").read_text().splitlines():
            with self.assertRaises(ChildProcessError):
                os.waitpid(int(pid), os.WNOHANG)

    def assert_rejected(self, **config):
        self.configure(**config)
        with self.assertRaises((RuntimeError, TimeoutError)):
            self.run_child(timeout=0.2)
        self.assertFalse(self.output.exists())
        self.assert_children_reaped()

    def test_success_obeys_coordinated_phase_order(self):
        result = self.run_child(workers=4, passes=3)
        self.assertEqual((self.world / "trace").read_text().splitlines(),
                         ["ready", "go", "measured", "verify", "verified", "exit"])
        self.assertEqual(result["measured"]["completed_loads"], 6)
        self.assertEqual(json.loads(self.output.read_text()), result)
        self.assert_children_reaped()

    def test_wait4_includes_teardown_cpu_and_peak_memory(self):
        self.configure(mode="usage")
        result = self.run_child()
        self.assertGreater(result["whole_process_cpu_seconds"] - result["pre_exit_sample_cpu_seconds"], 0.07)
        self.assertGreater(result["whole_process_peak_rss_bytes"] - result["process_hwm_before_exit_bytes"], 20 * 1024 * 1024)
        self.assertGreater(result["whole_process_wall_seconds"], result["driver_timed_window_wall_seconds"])
        self.assert_children_reaped()

    def test_wrong_phase_terminates_and_reaps_child(self):
        self.assert_rejected(mode="wrong_phase")

    def test_timeout_terminates_and_reaps_child(self):
        started = time.monotonic()
        self.assert_rejected(mode="timeout")
        self.assertLess(time.monotonic() - started, 3)

    def test_nonzero_exit_does_not_publish_result(self):
        self.assert_rejected(mode="nonzero")

    def test_teardown_timeout_terminates_and_reaps_child(self):
        started = time.monotonic()
        self.assert_rejected(mode="teardown_timeout")
        self.assertLess(time.monotonic() - started, 3)

    def test_early_exit_is_reaped(self):
        self.assert_rejected(mode="early_exit")

    def test_malformed_events_are_rejected(self):
        for mode in ("invalid_json", "non_object", "oversized"):
            with self.subTest(mode=mode):
                self.output = self.root / (mode + ".json")
                self.assert_rejected(mode=mode)

    def test_event_flood_does_not_leak_reader_thread(self):
        before = set(threading.enumerate())
        self.assert_rejected(mode="flood")
        self.assertEqual(set(threading.enumerate()) - before, set())

    def test_reader_start_failure_reaps_actual_child(self):
        pid_file = self.world / "pids"

        def fail_after_child_started():
            deadline = time.monotonic() + 2
            while not pid_file.exists() or not pid_file.stat().st_size:
                if time.monotonic() >= deadline:
                    self.fail("child did not start before reader failure")
                time.sleep(0.005)
            raise RuntimeError("reader start failed")

        try:
            with patch.object(DRIVER.threading.Thread, "start", side_effect=fail_after_child_started):
                with self.assertRaisesRegex(RuntimeError, "reader start failed"):
                    self.run_child()
            self.assert_children_reaped()
            self.assertFalse(self.output.exists())
        finally:
            if pid_file.exists():
                for pid in map(int, pid_file.read_text().splitlines()):
                    try:
                        if os.waitpid(pid, os.WNOHANG) == (0, 0):
                            os.kill(pid, signal.SIGKILL)
                            os.waitpid(pid, 0)
                    except ChildProcessError:
                        pass

    def test_oversized_selection_is_rejected_before_starting_child(self):
        with self.selection.open("ab") as file:
            for _ in range(128):
                file.write(b" " * 65536)
        with self.assertRaisesRegex(RuntimeError, "selection.*8 MiB"):
            self.run_child()
        self.assertFalse((self.world / "pids").exists())
        self.assertFalse(self.output.exists())

    def test_worker_and_pass_mismatches_are_rejected(self):
        for phase in ("ready", "measured"):
            for field in ("workers", "passes"):
                with self.subTest(phase=phase, field=field):
                    self.output = self.root / (phase + field + ".json")
                    self.assert_rejected(**{phase: {field: 2}})

    def test_boolean_worker_or_pass_is_rejected(self):
        for phase in ("ready", "measured"):
            for field in ("workers", "passes"):
                with self.subTest(phase=phase, field=field):
                    self.output = self.root / (phase + field + ".json")
                    self.assert_rejected(**{phase: {field: True}})

    def test_completed_loads_must_match_requested_selection(self):
        self.assert_rejected(measured={"completed_loads": 1})

    def test_invalid_measurements_are_not_published(self):
        for field in ("timed_loop_wall_seconds", "chunks_per_second"):
            for number, value in enumerate((0, -1, float("nan"), float("inf"), True, "1")):
                with self.subTest(field=field, value=value):
                    self.output = self.root / (field + str(number) + ".json")
                    self.assert_rejected(measured={field: value})

    def test_throughput_must_match_completed_loads_and_wall_time(self):
        self.assert_rejected(measured={"timed_loop_wall_seconds": 1.0, "chunks_per_second": 500.0})

    def test_measured_selection_count_must_match_ready(self):
        self.assert_rejected(measured={"selected_chunks": 3})

    def test_consistent_wrong_count_is_rejected_against_selection_file(self):
        self.assert_rejected(ready={"selected_chunks": 3},
                             measured={"selected_chunks": 3, "completed_loads": 3})

    def test_ready_selection_hash_is_validated(self):
        self.assert_rejected(ready={"provenance": {"selection_sha256": "0" * 64}})

    def test_selection_mutation_during_run_is_rejected(self):
        self.assert_rejected(mode="mutate_selection")

    def test_false_verification_is_rejected(self):
        self.assert_rejected(verified={"all_selected_chunks_match": False})

    def test_existing_output_is_preserved_without_starting_child(self):
        self.output.write_bytes(b"retain original")
        with self.assertRaises(FileExistsError):
            self.run_child()
        self.assertEqual(self.output.read_bytes(), b"retain original")
        self.assertFalse((self.world / "pids").exists())

    def test_existing_stderr_is_preserved(self):
        log = self.output.with_suffix(".stderr.log")
        log.write_bytes(b"retain original errors")
        with self.assertRaises(FileExistsError):
            self.run_child()
        self.assertEqual(log.read_bytes(), b"retain original errors")
        self.assertFalse((self.world / "pids").exists())

    def call_main(self, output):
        arguments = ["benchmark-world-load.py", "--binary", str(self.binary),
                     "--world", str(self.world), "--selection", str(self.selection),
                     "--settings", str(self.settings), "--output", str(output),
                     "--passes", "1", "--repeats", "3", "--timeout", "2"]
        with patch.object(sys, "argv", arguments), contextlib.redirect_stdout(io.StringIO()):
            DRIVER.main()

    def test_main_rotates_worker_order_and_publishes_bound_inputs(self):
        output = self.root / "runs"
        self.call_main(output)
        report = json.loads((output / "summary.json").read_text())
        self.assertEqual(report["metadata"]["worker_counts"], [1, 4])
        self.assertEqual([entry["runs"] for entry in report["summary"]], [3, 3])
        for key, source in (("binary_sha256", self.binary), ("settings_sha256", self.settings),
                            ("selection_sha256", self.selection)):
            self.assertEqual(report["metadata"][key], hashlib.sha256(source.read_bytes()).hexdigest())
        self.assertEqual(len(report["results"]), 6)
        pids = [int(pid) for pid in (self.world / "pids").read_text().splitlines()]
        rows = [json.loads(path.read_text()) for path in output.glob("workers-*.json")]
        by_pid = {row["pid"]: row for row in rows}
        self.assertEqual([by_pid[pid]["measured"]["workers"] for pid in pids], [1, 4, 4, 1, 1, 4])
        self.assert_children_reaped()

    def test_input_mutation_between_real_runs_prevents_summary(self):
        original_run = DRIVER.one_run
        for name in ("binary", "selection", "settings"):
            source = getattr(self, name)
            original = source.read_bytes()
            output = self.root / ("changed-" + name)
            with self.subTest(input=name):
                def run_then_change(*args, **kwargs):
                    result = original_run(*args, **kwargs)
                    source.write_bytes(source.read_bytes() + b"\n")
                    return result

                try:
                    with patch.object(DRIVER, "one_run", side_effect=run_then_change):
                        with self.assertRaisesRegex(RuntimeError, "changed"):
                            self.call_main(output)
                    self.assertFalse((output / "summary.json").exists())
                    self.assertEqual(len(list(output.glob("workers-*.json"))), 1)
                    self.assert_children_reaped()
                finally:
                    source.write_bytes(original)

    def test_existing_output_directory_is_not_reused(self):
        output = self.root / "existing"
        output.mkdir()
        (output / "retain").write_bytes(b"original")
        with self.assertRaises(FileExistsError):
            self.call_main(output)
        self.assertEqual((output / "retain").read_bytes(), b"original")
        self.assertFalse((self.world / "pids").exists())


if __name__ == "__main__":
    unittest.main()
