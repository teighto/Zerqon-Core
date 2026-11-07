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
            if (line.contains("\"op\":\"load\"")) {
                for (String candidate : new String[]{"timeout", "oversize", "wrong_id", "bad_json", "eof", "stderr", "bad_action", "startup_timeout", "pid", "charset"}) {
                    if (line.contains("java-fixture-" + candidate)) mode = candidate;
                }
                if (mode.equals("startup_timeout")) Thread.sleep(60_000);
                respond(id, true, "[{\"action\":\"register_command\",\"name\":\"jhello\",\"description\":\"fixture\",\"permission\":null}]");
            } else if (line.contains("\"op\":\"shutdown\"")) {
                respond(id, false, "[]");
                return;
            } else if (line.contains("\"op\":\"list\"")) {
                respond(id, true, "[]");
            } else {
                switch (mode) {
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
}
