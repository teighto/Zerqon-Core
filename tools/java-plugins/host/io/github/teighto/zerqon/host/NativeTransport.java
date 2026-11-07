package io.github.teighto.zerqon.host;

import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import io.github.teighto.zerqon.api.NativeQueries;
import io.github.teighto.zerqon.api.NativeQueryBinding;
import java.io.IOException;
import java.io.InputStream;
import java.io.PrintStream;
import java.nio.ByteBuffer;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.util.Locale;

final class NativeTransport {
    private static final int MAX_CALLS = 128;
    private static final int MAX_CALL_BYTES = 65_536;
    private static final int MAX_RESULT_BYTES = 262_144;
    private static final int MAX_RESULT_TOTAL_BYTES = 1_048_576;
    private final Thread owner = Thread.currentThread();
    private final PrintStream output;
    private final InputStream input;
    private boolean negotiated;
    private boolean operationsStarted;
    private boolean poisoned;
    private boolean active;
    private long parent;
    private long calls;
    private int resultBytes;

    NativeTransport(PrintStream output, InputStream input) { this.output = output; this.input = input; }

    JsonObject negotiate(JsonElement id, JsonObject request) {
        Wire.fields(request, "id", "op", "min_protocol", "max_protocol");
        if (operationsStarted || negotiated) throw new IllegalStateException("Protocol negotiation must precede other operations and occur once");
        Wire.integer(request, "id", 1, Long.MAX_VALUE);
        long minimum = Wire.integer(request, "min_protocol", 1, Integer.MAX_VALUE);
        long maximum = Wire.integer(request, "max_protocol", 1, Integer.MAX_VALUE);
        if (minimum > 2 || maximum < 2 || minimum > maximum) throw new IllegalArgumentException("Host protocol 2 is not in the requested range");
        negotiated = true;
        JsonObject response = new JsonObject();
        response.add("id", id);
        response.addProperty("ok", true);
        response.add("results", new JsonArray());
        response.addProperty("protocol", 2);
        JsonArray capabilities = new JsonArray();
        capabilities.add("native_queries");
        response.add("capabilities", capabilities);
        return response;
    }

    void begin(JsonObject request, String operation) {
        if (Thread.currentThread() != owner || active) throw new IllegalStateException("Invalid native parent context");
        operationsStarted = true;
        if (negotiated) parent = Wire.integer(request, "id", 1, Long.MAX_VALUE);
        calls = 0;
        resultBytes = 0;
        active = operation.equals("load") || operation.equals("event") || operation.equals("shutdown");
    }

    void end() { active = false; }

    NativeQueryBinding.Scope bind(String plugin) {
        if (!plugin.matches("[A-Za-z0-9_-]{1,64}")) throw new IllegalArgumentException("Invalid native plugin identity");
        return NativeQueryBinding.bind(() -> query(plugin));
    }

    void checkHealthy() throws IOException {
        if (poisoned) throw new IOException("Native query protocol is no longer synchronized");
    }

    private NativeQueries.ServerSnapshot query(String plugin) {
        if (Thread.currentThread() != owner || !active) throw new IllegalStateException("Native queries require the active host callback thread");
        if (!negotiated) throw new NativeQueries.NativeQueryException(NativeQueries.ErrorCode.UNSUPPORTED, "Native queries require host protocol 2");
        if (poisoned) throw failure("Native query protocol is no longer synchronized", null);
        if (calls == MAX_CALLS) throw failure("Native query limit exceeded", null);
        long call = ++calls;
        JsonObject operation = new JsonObject();
        operation.addProperty("query", "server_snapshot");
        JsonObject request = new JsonObject();
        request.addProperty("kind", "native_call");
        request.addProperty("parent_id", parent);
        request.addProperty("call_id", call);
        request.addProperty("plugin", plugin);
        request.add("operation", operation);
        String encoded = request.toString();
        if (encoded.getBytes(StandardCharsets.UTF_8).length > MAX_CALL_BYTES) throw failure("Native query frame exceeds limit", null);
        output.println(encoded);
        output.flush();
        if (output.checkError()) throw failure("Native query output closed", null);
        JsonObject result;
        try {
            byte[] line = Wire.readLine(input, MAX_RESULT_BYTES);
            if (line == null) throw new IOException("Native query input closed");
            resultBytes = Math.addExact(resultBytes, line.length);
            if (resultBytes > MAX_RESULT_TOTAL_BYTES) throw new IOException("Native query results exceed parent limit");
            String text = StandardCharsets.UTF_8.newDecoder().onMalformedInput(CodingErrorAction.REPORT)
                    .onUnmappableCharacter(CodingErrorAction.REPORT).decode(ByteBuffer.wrap(line)).toString();
            JsonObject response = Wire.parse(text);
            Wire.fields(response, "kind", "parent_id", "call_id", "result");
            if (!"native_result".equals(Wire.string(response, "kind", 32))
                    || Wire.integer(response, "parent_id", 1, Long.MAX_VALUE) != parent
                    || Wire.integer(response, "call_id", 1, Long.MAX_VALUE) != call) {
                throw new IOException("Native query response identity mismatch");
            }
            result = Wire.object(response, "result");
            if (Wire.bool(result, "ok")) {
                Wire.fields(result, "ok", "value");
                JsonObject value = Wire.object(result, "value");
                Wire.fields(value, "server_version", "minecraft_version", "protocol", "max_players", "online_players", "tick");
                return new NativeQueries.ServerSnapshot(Wire.string(value, "server_version", 128), Wire.string(value, "minecraft_version", 128),
                        (int) Wire.integer(value, "protocol", 0, Integer.MAX_VALUE), (int) Wire.integer(value, "max_players", 1, 1_000_000),
                        (int) Wire.integer(value, "online_players", 0, 1_000_000), Wire.integer(value, "tick", 0, Long.MAX_VALUE));
            }
            Wire.fields(result, "ok", "error");
            JsonObject error = Wire.object(result, "error");
            Wire.fields(error, "code", "message");
            String codeName = Wire.string(error, "code", 32);
            NativeQueries.ErrorCode code = NativeQueries.ErrorCode.valueOf(codeName.toUpperCase(Locale.ROOT));
            if (!code.name().toLowerCase(Locale.ROOT).equals(codeName)) throw new IllegalArgumentException("Invalid native error code");
            String message = Wire.string(error, "message", 1024);
            if (message.isEmpty() || message.codePoints().anyMatch(Character::isISOControl)) throw new IllegalArgumentException("Invalid native error message");
            throw new NativeQueries.NativeQueryException(code, message);
        } catch (NativeQueries.NativeQueryException exception) {
            throw exception;
        } catch (IOException | RuntimeException exception) {
            throw failure("Invalid native query response", exception);
        }
    }

    private IllegalStateException failure(String message, Throwable cause) {
        poisoned = true;
        return new IllegalStateException(message, cause);
    }
}
