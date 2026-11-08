"""Linux process driver for verified warm imported-block load/decode runs."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import queue
import statistics
import subprocess
import threading
import time
import tomllib


MAX_SELECTION_BYTES = 8 * 1024 * 1024


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            result.update(block)
    return result.hexdigest()


def process_sample(pid):
    root = Path("/proc") / str(pid)
    fields = (root / "stat").read_text().rsplit(")", 1)[1].split()
    ticks = os.sysconf("SC_CLK_TCK")
    status = {}
    for line in (root / "status").read_text().splitlines():
        if line.startswith(("VmRSS:", "VmHWM:")):
            name, amount, _unit = line.split()
            status[name[:-1]] = int(amount) * 1024
    io = {}
    try:
        io = {name.rstrip(":"): int(value) for name, value in (line.split() for line in (root / "io").read_text().splitlines())}
    except OSError:
        pass
    return {
        "cpu_seconds": (int(fields[11]) + int(fields[12])) / ticks,
        "threads": int(fields[17]), "rss_bytes": status.get("VmRSS"),
        "process_hwm_bytes": status.get("VmHWM"), "io": io,
    }


def private_memory(pid):
    try:
        values = {}
        for line in (Path("/proc") / str(pid) / "smaps_rollup").read_text().splitlines():
            if line.startswith(("Pss:", "Private_Clean:", "Private_Dirty:")):
                key, amount, _unit = line.split()
                values[key[:-1]] = int(amount) * 1024
        return {"pss_bytes": values.get("Pss"), "private_bytes": values.get("Private_Clean", 0) + values.get("Private_Dirty", 0)}
    except OSError:
        return None


def stream_events(stream, events, stopped):
    def publish(value):
        while not stopped.is_set():
            try:
                events.put(value, timeout=0.02)
                return True
            except queue.Full:
                continue
        return False

    try:
        while not stopped.is_set():
            line = stream.readline(131073)
            if not line:
                break
            if len(line) > 131072 or not line.endswith("\n"):
                raise ValueError("benchmark emitted an oversized/incomplete event")
            if not publish(json.loads(line)):
                break
    except Exception as error:
        publish({"reader_error": str(error)})
    finally:
        publish(None)


def record_sample(samples, sample):
    samples["count"] += 1
    samples["peak_rss_bytes"] = max(samples["peak_rss_bytes"], sample["rss_bytes"] or 0)
    samples["max_threads"] = max(samples["max_threads"], sample["threads"])


def wait_event(process, events, expected, timeout, samples=None):
    deadline = time.monotonic() + timeout
    while True:
        if samples is not None:
            record_sample(samples, process_sample(process.pid))
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"benchmark did not emit {expected}")
        try:
            value = events.get(timeout=min(0.02, remaining))
        except queue.Empty:
            if process.poll() is not None:
                raise RuntimeError(f"benchmark exited before {expected}: {process.returncode}")
            continue
        if not isinstance(value, dict) or value.get("event") != expected:
            raise RuntimeError(f"unexpected benchmark event while waiting for {expected}: {value}")
        return value


def send(process, message):
    process.stdin.write(message + "\n")
    process.stdin.flush()


def reap_usage(process, timeout):
    deadline = time.monotonic() + timeout
    while True:
        try:
            pid, status, usage = os.wait4(process.pid, os.WNOHANG)
        except ChildProcessError as error:
            raise RuntimeError("benchmark child was reaped without its resource usage") from error
        if pid == process.pid:
            process.returncode = os.waitstatus_to_exitcode(status)
            return process.returncode, usage
        if time.monotonic() >= deadline:
            raise TimeoutError("benchmark process did not finish teardown")
        time.sleep(0.01)


def assert_inputs_unchanged(inputs):
    for path, expected in inputs.items():
        try:
            unchanged = digest(path) == expected
        except OSError:
            unchanged = False
        if not unchanged:
            raise RuntimeError(f"benchmark input changed: {path}")


def validate_phase(value, workers, passes, selected_chunks, selection_hash):
    expected = {"workers": workers, "passes": passes, "selected_chunks": selected_chunks}
    for key, count in expected.items():
        if type(value.get(key)) is not int or value[key] != count:
            raise RuntimeError(f"benchmark reported another {key} configuration")
    provenance = value.get("provenance")
    if not isinstance(provenance, dict) or provenance.get("selection_sha256") != selection_hash:
        raise RuntimeError("benchmark reported another selection hash")


def one_run(binary, world, selection, workers, passes, output, timeout):
    if output.exists() or output.is_symlink():
        raise FileExistsError(f"benchmark output already exists: {output}")
    with selection.open("rb") as file:
        if os.fstat(file.fileno()).st_size > MAX_SELECTION_BYTES:
            raise RuntimeError("benchmark selection exceeds 8 MiB")
        selection_bytes = file.read(MAX_SELECTION_BYTES + 1)
    if len(selection_bytes) > MAX_SELECTION_BYTES:
        raise RuntimeError("benchmark selection exceeds 8 MiB")
    selection_data = json.loads(selection_bytes)
    chunks = selection_data.get("chunks") if isinstance(selection_data, dict) else None
    if not isinstance(chunks, list) or not 1 <= len(chunks) <= 4096:
        raise RuntimeError("benchmark selection must contain 1..4096 chunks")
    selected_chunks = len(chunks)
    selection_hash = hashlib.sha256(selection_bytes).hexdigest()
    inputs = {binary: digest(binary), selection: selection_hash}
    command = [str(binary), str(world), str(selection), str(workers), str(passes), "--coordinated"]
    stderr_path = output.with_suffix(".stderr.log")
    process_started = time.monotonic()
    with stderr_path.open("x", encoding="utf-8") as errors:
        events = queue.Queue(maxsize=8)
        stopped = threading.Event()
        reader = None
        process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors, text=True, encoding="utf-8", errors="strict", bufsize=1)
        try:
            reader = threading.Thread(target=stream_events, args=(process.stdout, events, stopped), daemon=True)
            reader.start()
            ready = wait_event(process, events, "ready", timeout)
            validate_phase(ready, workers, passes, selected_chunks, selection_hash)
            baseline = process_sample(process.pid)
            baseline_private = private_memory(process.pid)
            samples = {"count": 0, "peak_rss_bytes": 0, "max_threads": 0}
            record_sample(samples, baseline)
            go_wall = time.monotonic()
            send(process, "go")
            measured = wait_event(process, events, "measured", timeout, samples)
            after = process_sample(process.pid)
            measured_private = private_memory(process.pid)
            driver_window = time.monotonic() - go_wall
            record_sample(samples, after)
            validate_phase(measured, workers, passes, selected_chunks, selection_hash)
            if type(measured.get("completed_loads")) is not int or measured["completed_loads"] != selected_chunks * passes:
                raise RuntimeError("benchmark did not complete the requested selection")
            for key in ("timed_loop_wall_seconds", "chunks_per_second"):
                value = measured.get(key)
                if type(value) not in (int, float) or value <= 0 or not math.isfinite(value):
                    raise RuntimeError(f"benchmark reported invalid {key}")
            expected_rate = measured["completed_loads"] / measured["timed_loop_wall_seconds"]
            if not math.isclose(measured["chunks_per_second"], expected_rate, rel_tol=1e-9):
                raise RuntimeError("benchmark throughput differs from completed loads and wall time")
            send(process, "verify")
            verified = wait_event(process, events, "verified", timeout)
            final_sample = process_sample(process.pid)
            send(process, "exit")
            code, usage = reap_usage(process, min(timeout, 10))
            process_finished = time.monotonic()
            if code or verified.get("all_selected_chunks_match") is not True:
                raise RuntimeError(f"benchmark verification/exit failed: {code}")
            if verified.get("selection_sha256") != selection_hash:
                raise RuntimeError("benchmark verified another selection hash")
            assert_inputs_unchanged(inputs)
            timed_cpu = after["cpu_seconds"] - baseline["cpu_seconds"]
            result = {
                "command": command, "pid": process.pid, "ready": ready, "measured": measured, "verified": verified,
                "timed_boundary_cpu_seconds": timed_cpu,
                "timed_cpu_percent_of_one_core": 100 * timed_cpu / measured["timed_loop_wall_seconds"],
                "driver_timed_window_wall_seconds": driver_window,
                "whole_process_wall_seconds": process_finished - process_started,
                "whole_process_cpu_seconds": usage.ru_utime + usage.ru_stime,
                "whole_process_user_cpu_seconds": usage.ru_utime,
                "whole_process_system_cpu_seconds": usage.ru_stime,
                "whole_process_peak_rss_bytes": usage.ru_maxrss * 1024,
                "pre_exit_sample_cpu_seconds": final_sample["cpu_seconds"],
                "baseline_rss_bytes": baseline["rss_bytes"],
                "timed_peak_sampled_rss_bytes": samples["peak_rss_bytes"],
                "process_hwm_after_measurement_bytes": after["process_hwm_bytes"],
                "process_hwm_before_exit_bytes": final_sample["process_hwm_bytes"],
                "sample_interval_seconds": 0.02, "memory_samples": samples["count"], "observed_threads_max": samples["max_threads"],
                "baseline_private": baseline_private, "after_measurement_private": measured_private,
                "timed_boundary_io_counters_delta": {key: after["io"].get(key, 0) - value for key, value in baseline["io"].items()},
                "memory_note": "Sampled timed RSS can miss short peaks; /proc VmHWM is sampled before exit. wait4 whole-process maxRSS includes teardown. Neither is decoded-payload size.",
                "cpu_note": "Timed CPU is sampled at go/measured process boundaries; whole-process CPU includes untimed setup, verification and teardown. Rust wall measures only the load loop.",
            }
            with output.open("x", encoding="utf-8") as file:
                json.dump(result, file, indent=2)
                file.write("\n")
            return result
        finally:
            stopped.set()
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            process.stdin.close()
            if reader is not None and reader.ident is not None:
                reader.join(timeout=2)
            process.stdout.close()


def distribution(values):
    return {"median": statistics.median(values), "minimum": min(values), "maximum": max(values), "spread": max(values) - min(values), "values": values}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--world", type=Path, required=True)
    parser.add_argument("--selection", type=Path, required=True)
    parser.add_argument("--settings", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--passes", type=int, default=100)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=600)
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("this process-metrics driver requires Linux /proc")
    if not 1 <= args.passes <= 10000 or not 3 <= args.repeats <= 20 or not 1 <= args.timeout <= 1800:
        parser.error("passes/repeats/timeout outside limits")
    binary = args.binary.resolve(strict=True)
    world = args.world.resolve(strict=True)
    selection = args.selection.resolve(strict=True)
    settings_path = args.settings.resolve(strict=True)
    settings_bytes = settings_path.read_bytes()
    settings = tomllib.loads(settings_bytes.decode("utf-8"))
    configured = settings.get("world_threads")
    if type(configured) is not int or not 1 <= configured <= 256:
        parser.error("settings must contain an explicit world_threads value")
    counts = list(dict.fromkeys([1, 4, configured]))
    inputs = {binary: digest(binary), selection: digest(selection),
              settings_path: hashlib.sha256(settings_bytes).hexdigest()}
    args.output.mkdir(parents=True, exist_ok=False)
    stat = os.statvfs(world)
    if stat.f_bavail * stat.f_frsize < 4_000_000_000:
        raise RuntimeError("less than 4GB free; benchmark not started")
    cpu_model = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), None)
    try:
        mount = json.loads(subprocess.check_output(["findmnt", "--json", "--target", str(world), "--output", "TARGET,SOURCE,FSTYPE"], text=True))
    except (OSError, subprocess.SubprocessError, ValueError):
        mount = None
    metadata = {
        "binary_sha256": inputs[binary], "binary_bytes": binary.stat().st_size,
        "selection_sha256": inputs[selection], "settings_sha256": inputs[settings_path],
        "configured_world_threads": configured, "worker_counts": counts,
        "kernel": platform.release(), "machine": platform.machine(), "cpu_model": cpu_model,
        "logical_cpus": os.cpu_count(), "affinity_cpus": sorted(os.sched_getaffinity(0)),
        "storage": mount, "filesystem_block_bytes": stat.f_frsize,
        "db_bytes_before": (world / "chunks.redb").stat().st_size,
        "cache_policy": "new process per run; explicit untimed DB warmup; OS page cache not controlled or flushed",
    }
    rows = []
    for repeat in range(args.repeats):
        order = counts[repeat % len(counts):] + counts[:repeat % len(counts)]
        for workers in order:
            assert_inputs_unchanged(inputs)
            path = args.output / f"workers-{workers}-run-{repeat + 1}.json"
            rows.append(one_run(binary, world, selection, workers, args.passes, path, args.timeout))
            assert_inputs_unchanged(inputs)
    summary = []
    for workers in counts:
        group = [row for row in rows if row["measured"]["workers"] == workers]
        summary.append({
            "workers": workers, "runs": len(group),
            "chunks_per_second": distribution([row["measured"]["chunks_per_second"] for row in group]),
            "timed_loop_wall_seconds": distribution([row["measured"]["timed_loop_wall_seconds"] for row in group]),
            "timed_boundary_cpu_seconds": distribution([row["timed_boundary_cpu_seconds"] for row in group]),
            "timed_peak_sampled_rss_bytes": distribution([row["timed_peak_sampled_rss_bytes"] for row in group]),
            "whole_process_peak_rss_bytes": distribution([row["whole_process_peak_rss_bytes"] for row in group]),
            "whole_process_cpu_seconds": distribution([row["whole_process_cpu_seconds"] for row in group]),
        })
    report = {"metadata": metadata, "db_bytes_after": (world / "chunks.redb").stat().st_size, "summary": summary, "results": [str(path.name) for path in sorted(args.output.glob("workers-*.json"))]}
    assert_inputs_unchanged(inputs)
    with (args.output / "summary.json").open("x", encoding="utf-8") as output:
        json.dump(report, output, indent=2)
        output.write("\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
