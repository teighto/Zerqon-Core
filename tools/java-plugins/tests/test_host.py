import argparse
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[3]
JAVA = None
FIXTURES = None


class Host:
    def __init__(self, java_args=(), paper=False):
        libraries = ROOT / "libraries"
        paths = [libraries / "zerqon-java-host-0.0.1.jar"]
        if paper:
            paths.extend(sorted((ROOT / "libraries/paper").glob("*.jar")))
        paths.append(libraries / "gson-2.13.2.jar")
        cp = os.pathsep.join(map(str, paths))
        self.process = subprocess.Popen([JAVA, "-Xms16M", "-Xmx128M", *java_args, "-cp", cp, "io.github.teighto.zerqon.host.PluginHost"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8")
        self.process.stdin.reconfigure(newline="\n")
        self.lines = queue.Queue()
        self.errors = []
        self.reader = threading.Thread(target=self.read_output, daemon=True)
        self.reader.start()
        self.logger = threading.Thread(target=self.read_errors, daemon=True)
        self.logger.start()

    def read_output(self):
        for line in self.process.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def read_errors(self):
        for line in self.process.stderr:
            self.errors.append(line)

    def request(self, native=None, **request):
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        while True:
            line = self.lines.get(timeout=10)
            if line is None:
                raise AssertionError("host exited: " + "".join(self.errors))
            response = json.loads(line)
            if response.get("kind") != "native_call":
                break
            if native is None:
                raise AssertionError("unexpected native query: " + line)
            result = native(response)
            self.process.stdin.write((result if isinstance(result, str) else json.dumps(result)) + "\n")
            self.process.stdin.flush()
        if response["id"] != request["id"]:
            raise AssertionError("mismatched request id")
        return response

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=10)
        self.reader.join(timeout=2)
        self.logger.join(timeout=2)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            stream.close()


class HostTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.plugins = Path(self.directory.name)
        shutil.copy(ROOT / "dist/hello-java-plugin-0.0.1.jar", self.plugins / "hello.jar")
        self.host = Host()
        self.addCleanup(self.directory.cleanup)
        self.addCleanup(self.host.close)

    def load(self):
        return self.host.request(id=1, op="load", directory=str(self.plugins), server_version="0.0.1")

    def test_protocol_v2_negotiates_without_changing_legacy_plugin_results(self):
        hello = self.host.request(id=1, op="hello", min_protocol=2, max_protocol=2)
        self.assertEqual(hello, {"id": 1, "ok": True, "results": [], "protocol": 2, "capabilities": ["native_queries"]})
        loaded = self.host.request(id=2, op="load", directory=str(self.plugins), server_version="0.0.1")
        self.assertTrue(loaded["ok"])
        response = self.host.request(id=3, op="event", plugin="hello", event={"event": "command", "sender": "teighto", "sender_kind": "player", "command": "jhello", "args": []})
        self.assertEqual(response["results"][0]["actions"], [{"action": "reply", "message": "Hello, teighto! This plugin uses the Zerqon Java API."}])

    def test_protocol_negotiation_rejects_late_duplicate_and_unsupported_requests(self):
        for request in [{"id": 1, "op": "hello", "min_protocol": 3, "max_protocol": 4}, {"id": 2, "op": "hello", "min_protocol": 2, "max_protocol": 1}]:
            self.assertFalse(self.host.request(**request)["ok"])
        self.assertTrue(self.host.request(id=3, op="hello", min_protocol=2, max_protocol=2)["ok"])
        self.assertFalse(self.host.request(id=4, op="hello", min_protocol=2, max_protocol=2)["ok"])
        self.host.close()
        self.host = Host()
        self.addCleanup(self.host.close)
        self.load()
        self.assertFalse(self.host.request(id=2, op="hello", min_protocol=2, max_protocol=2)["ok"])

    def test_hello_example_exposes_native_snapshot_with_legacy_fallback(self):
        self.load()
        command = {"event": "command", "sender": "teighto", "sender_kind": "player", "command": "jserver", "args": []}
        legacy = self.host.request(id=2, op="event", plugin="hello", event=command)
        self.assertEqual(legacy["results"][0]["actions"], [{"action": "reply", "message": "Native server queries are unavailable (protocol 2 required)"}])
        self.host.close()
        self.host = Host()
        self.addCleanup(self.host.close)
        self.assertTrue(self.host.request(id=1, op="hello", min_protocol=2, max_protocol=2)["ok"])
        loaded = self.host.request(id=2, op="load", directory=str(self.plugins), server_version="0.0.1")
        self.assertIn({"action": "register_command", "name": "jserver", "description": "Show the native server snapshot", "permission": None}, loaded["results"][0]["actions"])
        native = self.host.request(native=self.native_result, id=3, op="event", plugin="hello", event=command)
        self.assertEqual(native["results"][0]["actions"], [{"action": "reply", "message": "Zerqon 0.0.1 | Minecraft 26.3 | Players 3/100 | Tick 42"}])

    def add_fixture(self, name, class_name="FaultyPlugin", api="1"):
        with zipfile.ZipFile(self.plugins / f"{name}.jar", "w") as jar:
            jar.writestr("META-INF/MANIFEST.MF", f"Manifest-Version: 1.0\nZerqon-Plugin: io.github.teighto.zerqon.fixtures.{class_name}\nZerqon-API: {api}\n\n")
            for file in FIXTURES.rglob("*.class"):
                jar.write(file, file.relative_to(FIXTURES).as_posix())

    def native_ready(self):
        self.add_fixture("native", "NativeQueryPlugin")
        self.assertTrue(self.host.request(id=1, op="hello", min_protocol=2, max_protocol=2)["ok"])
        self.assertTrue(self.host.request(id=2, op="load", directory=str(self.plugins), server_version="0.0.1")["ok"])

    def add_paper_native(self, filename, metadata):
        with zipfile.ZipFile(self.plugins / f"{filename}.jar", "w") as jar:
            for file in (ROOT / "target/paper-bridge/fixtures").rglob("NativePaperProbe*.class"):
                jar.write(file, file.relative_to(ROOT / "target/paper-bridge/fixtures").as_posix())
            jar.writestr("plugin.yml", f"name: {metadata}\nversion: 1\nmain: io.github.teighto.zerqon.fixtures.NativePaperProbe\napi-version: '26.3'\ncommands:\n  nativepaper: {{}}\n")

    @staticmethod
    def native_result(call, value=None, error=None):
        if value is None:
            value = {"server_version": "0.0.1", "minecraft_version": "26.3", "protocol": 777, "max_players": 100, "online_players": 3, "tick": 42}
        result = {"ok": True, "value": value} if error is None else {"ok": False, "error": error}
        return {"kind": "native_result", "parent_id": call["parent_id"], "call_id": call["call_id"], "result": result}

    def native_command(self, responder, command="native", request_id=3):
        return self.host.request(native=responder, id=request_id, op="event", plugin="native", event={"event": "command", "sender": "teighto", "sender_kind": "player", "command": command, "args": []})

    def test_native_query_roundtrip_and_non_inherited_thread_context(self):
        self.native_ready()
        calls = []
        def respond(call):
            calls.append(call)
            return self.native_result(call)
        result = self.native_command(respond)
        self.assertEqual(calls, [{"kind": "native_call", "parent_id": 3, "call_id": 1, "plugin": "native", "operation": {"query": "server_snapshot"}}])
        self.assertEqual(result["results"][0]["actions"], [{"action": "reply", "message": "0.0.1:26.3:777:100:3:42"}])
        thread = self.native_command(None, "thread", 4)
        self.assertEqual(thread["results"][0]["actions"], [{"action": "reply", "message": "thread rejected"}])

    def test_native_query_is_explicitly_unavailable_to_legacy_transport(self):
        self.add_fixture("native", "NativeQueryPlugin")
        self.load()
        result = self.native_command(None)
        self.assertEqual(result["results"][0]["actions"], [{"action": "reply", "message": "UNSUPPORTED"}])

    def test_native_service_errors_are_typed_and_do_not_poison_next_query(self):
        self.native_ready()
        calls = []
        def respond(call):
            calls.append(call["call_id"])
            return self.native_result(call, error={"code": "unavailable", "message": "service offline"}) if len(calls) == 1 else self.native_result(call)
        result = self.native_command(respond, "twice")
        self.assertEqual(calls, [1, 2])
        self.assertTrue(result["results"][0]["enabled"])
        self.assertEqual(result["results"][0]["actions"][0]["message"], "0.0.1:26.3:777:100:3:42")

    def test_native_calls_keep_one_sequence_across_lifecycle_plugins(self):
        for name in ["one", "two"]:
            self.add_fixture(name, "NativeLifecyclePlugin")
        self.assertTrue(self.host.request(id=1, op="hello", min_protocol=2, max_protocol=2)["ok"])
        for request in [{"id": 2, "op": "load", "directory": str(self.plugins), "server_version": "0.0.1"}, {"id": 3, "op": "event", "event": {"event": "tick", "tick": 1}}, {"id": 4, "op": "shutdown"}]:
            calls = []
            def respond(call):
                calls.append((call["parent_id"], call["call_id"], call["plugin"]))
                return self.native_result(call)
            self.assertTrue(self.host.request(native=respond, **request)["ok"])
            self.assertEqual(calls, [(request["id"], 1, "one"), (request["id"], 2, "two")])

    def test_native_query_count_is_bounded_even_when_plugin_catches_service_errors(self):
        self.native_ready()
        calls = []
        def respond(call):
            calls.append(call["call_id"])
            return self.native_result(call, error={"code": "capacity", "message": "budget"})
        with self.assertRaises(AssertionError):
            self.native_command(respond, "limit")
        self.assertEqual(calls, list(range(1, 129)))
        self.assertNotEqual(self.host.process.wait(timeout=10), 0)

    def test_mismatched_native_reply_poisons_host_even_if_plugin_catches_exception(self):
        self.native_ready()
        def respond(call):
            result = self.native_result(call)
            result["parent_id"] += 1
            return result
        with self.assertRaises(AssertionError):
            self.native_command(respond, "catch_protocol")
        self.assertNotEqual(self.host.process.wait(timeout=10), 0)

    def test_malformed_native_results_are_fatal_instead_of_fabricating_snapshot(self):
        variants = [
            ("extra", lambda response: response.update(extra=True)),
            ("call", lambda response: response.update(call_id=2)),
            ("kind", lambda response: response.update(kind="response")),
            ("missing", lambda response: response["result"].pop("value")),
            ("ok", lambda response: response["result"].update(ok="true")),
            ("online", lambda response: response["result"]["value"].update(online_players=101)),
            ("capacity", lambda response: response["result"]["value"].update(max_players=0)),
            ("tick", lambda response: response["result"]["value"].update(tick=9223372036854775808)),
            ("control", lambda response: response["result"]["value"].update(server_version="bad\nversion")),
            ("surrogate", lambda response: response["result"]["value"].update(server_version="\ud800")),
            ("version", lambda response: response["result"]["value"].update(server_version="")),
            ("error_code", lambda response: response.update(result={"ok": False, "error": {"code": "UNKNOWN", "message": "bad"}})),
            ("uppercase_code", lambda response: response.update(result={"ok": False, "error": {"code": "UNAVAILABLE", "message": "bad"}})),
            ("empty_error", lambda response: response.update(result={"ok": False, "error": {"code": "unavailable", "message": ""}})),
            ("control_error", lambda response: response.update(result={"ok": False, "error": {"code": "unavailable", "message": "bad\nmessage"}})),
        ]
        for name, mutate in variants:
            with self.subTest(name=name):
                self.host.close()
                self.host = Host()
                self.addCleanup(self.host.close)
                self.native_ready()
                def respond(call):
                    result = self.native_result(call)
                    mutate(result)
                    return result
                with self.assertRaises(AssertionError):
                    self.native_command(respond, "catch_protocol")
                self.assertNotEqual(self.host.process.wait(timeout=10), 0)

    def test_native_result_frame_and_parent_aggregate_are_bounded(self):
        for size, command, expected in [(262_145, "native", 1), (262_144, "aggregate", 5)]:
            self.host.close()
            self.host = Host()
            self.addCleanup(self.host.close)
            self.native_ready()
            calls = []
            def respond(call):
                calls.append(call["call_id"])
                result = json.dumps(self.native_result(call))
                return result + " " * (size - len(result))
            with self.assertRaises((AssertionError, BrokenPipeError, OSError)):
                self.native_command(respond, command)
            self.assertEqual(len(calls), expected)
            self.assertNotEqual(self.host.process.wait(timeout=10), 0)

    def test_paper_callbacks_use_file_identity_for_native_transport(self):
        self.host.close()
        self.host = Host(paper=True)
        self.addCleanup(self.host.close)
        self.add_paper_native("transport", "DifferentMetadataName")
        self.assertTrue(self.host.request(id=1, op="hello", min_protocol=2, max_protocol=2)["ok"])
        requests = [
            ({"id": 2, "op": "load", "directory": str(self.plugins), "server_version": "0.0.1"}, 2),
            ({"id": 3, "op": "event", "event": {"event": "tick", "tick": 1}}, 1),
            ({"id": 4, "op": "event", "plugin": "transport", "event": {"event": "command", "sender": "player", "sender_kind": "player", "command": "nativepaper", "args": []}}, 2),
            ({"id": 5, "op": "shutdown"}, 1),
        ]
        for request, expected in requests:
            calls = []
            def respond(call):
                calls.append(call)
                return self.native_result(call)
            response = self.host.request(native=respond, **request)
            self.assertTrue(response["ok"], response)
            self.assertEqual(len(calls), expected, response)
            self.assertTrue(all(call["plugin"] == "transport" for call in calls))
            self.assertEqual([call["call_id"] for call in calls], list(range(1, expected + 1)))

    def test_nested_paper_commands_switch_native_identity_and_restore_parent_scope(self):
        self.host.close()
        self.host = Host(paper=True)
        self.addCleanup(self.host.close)
        self.add_paper_native("file-a", "NativeA")
        self.add_paper_native("file-b", "NativeB")
        self.assertTrue(self.host.request(id=1, op="hello", min_protocol=2, max_protocol=2)["ok"])
        loaded = self.host.request(native=self.native_result, id=2, op="load", directory=str(self.plugins), server_version="0.0.1")
        self.assertTrue(all(result["enabled"] for result in loaded["results"]), loaded)
        calls = []
        def respond(call):
            calls.append((call["call_id"], call["plugin"]))
            return self.native_result(call)
        response = self.host.request(native=respond, id=3, op="event", plugin="file-a", event={"event": "command", "sender": "player", "sender_kind": "player", "command": "nativepaper", "args": ["nested", "nativeb:nativepaper"]})
        self.assertTrue(response["results"][0]["enabled"], response)
        self.assertEqual(calls, [(1, "file-b"), (2, "file-b"), (3, "file-a")])

    def test_real_plugin_command_and_clean_shutdown(self):
        loaded = self.load()
        self.assertTrue(loaded["ok"])
        actions = loaded["results"][0]["actions"]
        self.assertIn({"action": "register_command", "name": "jhello", "description": "A greeting from a Java plugin", "permission": None}, actions)
        response = self.host.request(id=18446744073709551615, op="event", plugin="hello", event={"event": "command", "sender": "teighto", "sender_kind": "player", "command": "jhello", "args": []})
        self.assertEqual(response["results"][0]["actions"], [{"action": "reply", "message": "Hello, teighto! This plugin uses the Zerqon Java API."}])
        shutdown = self.host.request(id=3, op="shutdown")
        self.assertTrue(shutdown["ok"])
        self.assertFalse(shutdown["results"][0]["enabled"])
        self.assertEqual(self.host.process.wait(timeout=10), 0)

    def test_faulty_plugin_is_disabled_without_corrupting_stdout(self):
        self.add_fixture("faulty")
        response = self.load()
        self.assertEqual(len(response["results"]), 2)
        response = self.host.request(id=2, op="event", event={"event": "command", "sender": "console", "sender_kind": "console", "command": "jhello", "args": []})
        results = {entry["name"]: entry for entry in response["results"]}
        self.assertFalse(results["faulty"]["enabled"])
        self.assertIn("fixture failure", results["faulty"]["error"])
        self.assertTrue(results["hello"]["enabled"])
        self.assertEqual(len(results["hello"]["actions"]), 1)
        response = self.host.request(id=3, op="event", event={"event": "tick", "tick": 1})
        self.assertEqual([entry["name"] for entry in response["results"]], ["hello"])
        self.assertTrue(any("fixture plugin stdout" in line for line in self.host.errors))

    def test_rejects_unknown_api_and_keeps_valid_plugins(self):
        self.add_fixture("future", api="2")
        response = self.load()
        results = {entry["name"]: entry for entry in response["results"]}
        self.assertFalse(results["future"]["enabled"])
        self.assertIn("API", results["future"]["error"])
        self.assertTrue(results["hello"]["enabled"])

    def test_rejects_non_zerqon_jars_explicitly(self):
        with zipfile.ZipFile(self.plugins / "paper.jar", "w") as jar:
            jar.writestr("plugin.yml", "name: PaperExample\n")
        response = self.load()
        failed = next(entry for entry in response["results"] if entry["name"] == "paper")
        self.assertFalse(failed["enabled"])
        self.assertIn("Paper bridge unavailable", failed["error"])

    def test_invalid_event_does_not_disable_valid_plugin(self):
        self.load()
        invalid = self.host.request(id=2, op="event", event={"event": "not_real"})
        self.assertFalse(invalid["ok"])
        listing = self.host.request(id=3, op="list")
        self.assertTrue(listing["results"][0]["enabled"])

    def test_duplicate_load_does_not_replace_plugin(self):
        self.load()
        response = self.load()
        self.assertIn("already loaded", response["results"][0]["error"])
        self.assertTrue(response["results"][0]["enabled"])

    def test_oversized_request_exits_without_allocating_unbounded_line(self):
        self.host.process.stdin.write("x" * 65537 + "\n")
        self.host.process.stdin.flush()
        self.assertNotEqual(self.host.process.wait(timeout=10), 0)

    def test_protocol_is_utf8_independent_of_default_stdout_encoding(self):
        self.host.close()
        self.host = Host(["-Dstdout.encoding=US-ASCII"])
        self.addCleanup(self.host.close)
        self.load()
        response = self.host.request(id=2, op="event", plugin="hello", event={"event": "command", "sender": "Привет 😀", "sender_kind": "player", "command": "jhello", "args": []})
        self.assertEqual(response["results"][0]["actions"][0]["message"], "Hello, Привет 😀! This plugin uses the Zerqon Java API.")

    def test_context_classloader_covers_construction_and_callbacks(self):
        self.add_fixture("context", class_name="ContextPlugin")
        response = self.load()
        context = next(entry for entry in response["results"] if entry["name"] == "context")
        self.assertTrue(context["enabled"], context)
        response = self.host.request(id=2, op="event", plugin="context", event={"event": "tick", "tick": 7})
        self.assertTrue(response["results"][0]["enabled"])

    def test_invalid_actions_disable_the_plugin(self):
        for index, command in enumerate(("flood", "long", "surrogate", "register")):
            self.add_fixture(f"bad-{index}")
        self.load()
        for index, command in enumerate(("flood", "long", "surrogate", "register")):
            response = self.host.request(id=index + 2, op="event", plugin=f"bad-{index}", event={"event": "command", "sender": "console", "sender_kind": "console", "command": command, "args": []})
            result = response["results"][0]
            self.assertFalse(result["enabled"], command)
            self.assertEqual(result["actions"], [])
            self.assertIn("error", result)

    def test_duplicate_json_fields_are_rejected(self):
        self.host.process.stdin.write('{"id":1,"op":"list","op":"shutdown"}\n')
        self.host.process.stdin.flush()
        response = json.loads(self.host.lines.get(timeout=10))
        self.assertFalse(response["ok"])
        self.assertIn("duplicate", response["error"])
        self.assertIsNone(self.host.process.poll())

    def test_invalid_file_name_is_rejected_before_loading_any_plugin(self):
        shutil.copy(self.plugins / "hello.jar", self.plugins / "invalid name.jar")
        response = self.load()
        self.assertFalse(response["ok"])
        self.assertEqual(self.host.request(id=2, op="list")["results"], [])

    def test_all_non_command_events_decode_and_reach_plugin(self):
        self.load()
        events = [{"event": "tick", "tick": 123}, {"event": "player_join", "uuid": "00000000-0000-0000-0000-000000000001", "name": "teighto"}, {"event": "player_quit", "uuid": "00000000-0000-0000-0000-000000000001", "name": "teighto"}]
        for index, event in enumerate(events):
            response = self.host.request(id=index + 2, op="event", plugin="hello", event=event)
            self.assertTrue(response["ok"])
            self.assertTrue(response["results"][0]["enabled"])
            self.assertEqual(response["results"][0]["actions"], [])

    def test_player_named_console_is_not_a_console_sender(self):
        self.add_fixture("sender", class_name="SenderPlugin")
        self.load()
        response = self.host.request(id=2, op="event", plugin="sender", event={"event": "command", "sender": "CONSOLE", "sender_kind": "player", "command": "who", "args": []})
        self.assertEqual(response["results"][0]["actions"], [{"action": "reply", "message": "PLAYER"}])
        missing = self.host.request(id=3, op="event", plugin="sender", event={"event": "command", "sender": "CONSOLE", "command": "who", "args": []})
        self.assertFalse(missing["ok"])

    def test_trusted_operator_flag_is_accepted_without_name_based_inference(self):
        self.add_fixture("sender", class_name="SenderPlugin")
        self.load()
        response = self.host.request(id=2, op="event", plugin="sender", event={"event": "command", "sender": "CONSOLE", "sender_kind": "player", "operator": False, "command": "who", "args": []})
        self.assertTrue(response["ok"], response)
        invalid = self.host.request(id=3, op="event", plugin="sender", event={"event": "command", "sender": "name", "sender_kind": "player", "operator": "true", "command": "who", "args": []})
        self.assertFalse(invalid["ok"])

    def test_paper_plugin_uses_real_api_through_host_json(self):
        self.host.close()
        self.host = Host(paper=True)
        self.addCleanup(self.host.close)
        shutil.copy(ROOT / "target/paper-bridge/paper-probe.jar", self.plugins / "paper.jar")
        loaded = self.load()
        paper = next(result for result in loaded["results"] if result["name"] == "paper")
        self.assertTrue(paper["enabled"], paper)
        self.assertIn({"action": "register_command", "name": "probe", "description": "", "permission": "zerqon.probe"}, paper["actions"])
        self.assertTrue(any(action.get("name") == "p" for action in paper["actions"]))
        for tick in (1, 2, 3):
            response = self.host.request(id=10 + tick, op="event", event={"event": "tick", "tick": tick})
            self.assertTrue(all(result["enabled"] for result in response["results"]))
        event = {"event": "command", "sender": "Operator", "sender_kind": "player", "operator": True, "command": "p", "args": []}
        response = self.host.request(id=20, op="event", plugin="paper", event=event)
        self.assertEqual(response["results"][0]["actions"], [{"action": "reply", "message": "hello:2:1:Operator:p"}])
        event.update(sender="CONSOLE", operator=False)
        denied = self.host.request(id=21, op="event", plugin="paper", event=event)
        self.assertEqual(denied["results"][0]["actions"], [{"action": "reply", "message": "Permission denied"}])
        shutdown = self.host.request(id=22, op="shutdown")
        self.assertTrue(all(not result["enabled"] for result in shutdown["results"]))
        self.assertEqual(self.host.process.wait(timeout=10), 0)

    def test_paper_hard_dependencies_load_before_dependents(self):
        self.host.close()
        self.host = Host(paper=True)
        self.addCleanup(self.host.close)
        shutil.copy(ROOT / "target/paper-bridge/base-probe.jar", self.plugins / "z-base.jar")
        shutil.copy(ROOT / "target/paper-bridge/dependent-probe.jar", self.plugins / "a-dependent.jar")
        results = self.load()["results"]
        self.assertTrue(all(result["enabled"] for result in results), results)
        self.assertEqual(len(results), 3)

    def test_custom_api_permissions_are_enforced_in_host(self):
        self.add_fixture("restricted", class_name="RestrictedPlugin")
        self.load()
        event = {"event": "command", "sender": "CONSOLE", "sender_kind": "player", "operator": False, "command": "private", "args": []}
        denied = self.host.request(id=2, op="event", plugin="restricted", event=event)
        self.assertEqual(denied["results"][0]["actions"], [{"action": "reply", "message": "Permission denied"}])
        event["operator"] = True
        allowed = self.host.request(id=3, op="event", plugin="restricted", event=event)
        self.assertEqual(allowed["results"][0]["actions"], [{"action": "reply", "message": "allowed:true"}])

    def test_paper_enable_failure_does_not_break_other_plugins(self):
        self.host.close()
        self.host = Host(paper=True)
        self.addCleanup(self.host.close)
        shutil.copy(ROOT / "target/paper-bridge/failing-probe.jar", self.plugins / "broken.jar")
        shutil.copy(ROOT / "target/paper-bridge/paper-probe.jar", self.plugins / "paper.jar")
        results = {item["name"]: item for item in self.load()["results"]}
        self.assertFalse(results["broken"]["enabled"])
        self.assertIn("intentional enable failure", results["broken"]["error"])
        self.assertTrue(results["paper"]["enabled"] and results["hello"]["enabled"])
        event = {"event": "command", "sender": "console", "sender_kind": "console", "command": "probe", "args": ["event"]}
        response = self.host.request(id=2, op="event", plugin="paper", event=event)
        self.assertEqual(response["results"][0]["actions"], [{"action": "reply", "message": "lowmonitor:true"}])


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--java", required=True)
    parser.add_argument("--fixtures", required=True)
    args, remaining = parser.parse_known_args()
    JAVA = args.java
    FIXTURES = Path(args.fixtures)
    unittest.main(argv=[__file__, *remaining])
