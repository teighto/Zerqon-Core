package io.github.teighto.zerqon.host;

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.util.regex.Pattern;

public final class PluginHost {
    private static final Pattern ID = Pattern.compile("\\\"id\\\":(\\d+)");

    public static void main(String[] args) throws Exception {
        String mode = "echo";
        var reader = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
        for (String line; (line = reader.readLine()) != null;) {
            var match = ID.matcher(line);
            if (!match.find()) throw new IllegalArgumentException("missing id");
            String id = match.group(1);
            if (line.contains("\"op\":\"hello\"")) {
                String source = PluginHost.class.getProtectionDomain().getCodeSource().getLocation().getPath();
                if (source.contains("no-protocol")) respond(id, true, "[]");
                else if (source.contains("stalled-protocol")) Thread.sleep(60_000);
                else if (source.contains("native-before-hello")) System.out.println(nativeCall(id, 1, "fixture", "server_snapshot"));
                else {
                    int protocol = source.contains("bad-protocol") ? 1 : 2;
                    String capability = source.contains("bad-capability") ? "arbitrary_files" : "native_queries";
                    System.out.println("{\"id\":" + id + ",\"ok\":true,\"results\":[],\"protocol\":" + protocol + ",\"capabilities\":[\"" + capability + "\"]}");
                }
            } else if (line.contains("\"op\":\"load\"")) {
                for (String candidate : new String[]{"timeout", "oversize", "wrong_id", "bad_json", "eof", "stderr", "bad_action", "startup_timeout", "pid", "charset", "native", "native_wrong_parent", "native_wrong_id", "native_unknown_plugin", "native_unknown_query", "native_unknown_field", "native_duplicate_field", "native_oversize", "native_duplicate", "native_limit", "native_error", "native_two", "native_stall", "native_list", "native_disabled", "native_players"}) {
                    if (line.contains("java-fixture-" + candidate + "\"")) mode = candidate;
                }
                if (mode.equals("startup_timeout")) Thread.sleep(60_000);
                respond(id, true, "[{\"action\":\"register_command\",\"name\":\"jhello\",\"description\":\"fixture\",\"permission\":null}]");
            } else if (line.contains("\"op\":\"shutdown\"")) {
                respond(id, false, "[]");
                return;
            } else if (line.contains("\"op\":\"list\"")) {
                if (mode.equals("native_list")) {
                    System.out.println(nativeCall(id, 1, "fixture", "server_snapshot"));
                    System.out.flush();
                    reader.readLine();
                }
                respond(id, !mode.equals("native_disabled"), "[]");
            } else {
                switch (mode) {
                    case "native", "native_error", "native_two", "native_stall", "native_disabled", "native_players" -> {
                        int count = mode.equals("native_two") ? 2 : 1;
                        for (int call = 1; call <= count; call++) {
                            System.out.println(nativeCall(id, call, "fixture", mode.equals("native_players") ? "online_players" : "server_snapshot"));
                            System.out.flush();
                            String response = reader.readLine();
                            if (mode.equals("native_error") || mode.equals("native_players")) {
                                if (response == null || !response.contains("\"ok\":false") || !response.contains("\"code\":")) throw new IllegalStateException("missing native error");
                                if (mode.equals("native_players") && !response.contains("\"code\":\"capacity\"")) throw new IllegalStateException("missing capacity error");
                            } else if (response == null || !response.contains("\"tick\":123") || !response.contains("\"online_players\":2")) throw new IllegalStateException("bad native result");
                        }
                        if (mode.equals("native_stall")) Thread.sleep(60_000);
                        respond(id, !mode.equals("native_disabled"), "[{\"action\":\"reply\",\"message\":\"" + (mode.equals("native_error") || mode.equals("native_players") ? "native-error" : "snapshot:123:2") + "\"}]");
                    }
                    case "native_wrong_parent", "native_wrong_id", "native_unknown_plugin", "native_unknown_query", "native_unknown_field", "native_duplicate_field", "native_oversize", "native_duplicate", "native_limit" -> {
                        int count = mode.equals("native_limit") ? 129 : mode.equals("native_duplicate") ? 2 : 1;
                        for (int call = 1; call <= count; call++) {
                            String parent = mode.equals("native_wrong_parent") ? "99999999" : id;
                            int child = mode.equals("native_wrong_id") ? 2 : mode.equals("native_duplicate") ? 1 : call;
                            String plugin = mode.equals("native_unknown_plugin") ? "unloaded" : mode.equals("native_oversize") ? "x".repeat(65_536) : "fixture";
                            String query = mode.equals("native_unknown_query") ? "read_arbitrary_file" : "server_snapshot";
                            String frame = nativeCall(parent, child, plugin, query);
                            if (mode.equals("native_unknown_field")) frame = frame.replace("\"operation\":", "\"extra\":1,\"operation\":");
                            if (mode.equals("native_duplicate_field")) frame = frame.replace("\"operation\":", "\"call_id\":1,\"operation\":");
                            System.out.println(frame);
                            System.out.flush();
                            if (reader.readLine() == null) return;
                        }
                        throw new IllegalStateException("invalid native frame accepted");
                    }
                    case "charset" -> {
                        if (!System.out.charset().equals(StandardCharsets.UTF_8)
                                || !System.err.charset().equals(StandardCharsets.UTF_8)) {
                            throw new IllegalStateException("JVM stdout and stderr must both be UTF-8");
                        }
                        System.err.println("Привет из stderr");
                        java.util.logging.Logger.getLogger("fixture").info("Привет из JUL");
                        respond(id, true, "[{\"action\":\"reply\",\"message\":\"Привет из Java\"}]");
                    }
                    case "pid" -> respond(id, true, "[{\"action\":\"reply\",\"message\":\"" + ProcessHandle.current().pid() + "\"}]");
                    case "timeout" -> Thread.sleep(60_000);
                    case "oversize" -> { System.out.print("x".repeat(8 * 1024 * 1024 + 1)); System.out.flush(); }
                    case "wrong_id" -> respond("999999", true, "[]");
                    case "bad_json" -> System.out.println("invalid json");
                    case "eof" -> { return; }
                    case "bad_action" -> respond(id, true, "[{\"action\":\"reply\",\"message\":\"" + "x".repeat(4097) + "\"}]");
                    case "stderr" -> {
                        byte[] noise = new byte[8192];
                        java.util.Arrays.fill(noise, (byte)'x');
                        for (int i = 0; i < 128; i++) System.err.write(noise);
                        System.err.flush();
                        respond(id, true, "[{\"action\":\"reply\",\"message\":\"received\"}]");
                    }
                    default -> respond(id, true, "[{\"action\":\"reply\",\"message\":\"received\"}]");
                }
            }
            System.out.flush();
        }
    }

    private static void respond(String id, boolean enabled, String actions) {
        System.out.println("{\"id\":" + id + ",\"ok\":true,\"results\":[{\"name\":\"fixture\",\"enabled\":" + enabled + ",\"actions\":" + actions + "}]}");
    }

    private static String nativeCall(String parent, int call, String plugin, String query) {
        return "{\"kind\":\"native_call\",\"parent_id\":" + parent + ",\"call_id\":" + call + ",\"plugin\":\"" + plugin + "\",\"operation\":{\"query\":\"" + query + "\"}}";
    }
}
