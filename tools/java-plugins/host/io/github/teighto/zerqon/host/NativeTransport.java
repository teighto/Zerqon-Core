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
import java.util.List;
import java.util.Optional;
import java.util.UUID;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.function.Function;
import io.github.teighto.zerqon.api.Event;

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
    private NativeQueries.PlayerSnapshot eventPlayer;
    private List<NativeQueries.PlayerSnapshot> onlinePlayers;
    private List<NativeQueries.WorldSnapshot> worlds;
    private final Map<String, Optional<NativeQueries.PlayerSnapshot>> players = new HashMap<>();

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
        eventPlayer = null;
        onlinePlayers = null;
        worlds = null;
        players.clear();
        calls = 0;
        resultBytes = 0;
        active = operation.equals("load") || operation.equals("event") || operation.equals("shutdown");
    }

    void end() { active = false; eventPlayer = null; onlinePlayers = null; worlds = null; players.clear(); }

    NativeQueries.PlayerSnapshot eventPlayer() { return eventPlayer; }

    void context(Event event, JsonObject request) {
        if (!request.has("context")) return;
        if (!negotiated) throw new IllegalArgumentException("Native context requires protocol 2");
        JsonObject context = Wire.object(request, "context");
        Wire.fields(context, "player");
        NativeQueries.PlayerSnapshot player = player(Wire.object(context, "player"));
        boolean matches = switch (event) {
            case Event.Command command -> command.senderKind() == Event.SenderKind.PLAYER && command.sender().equals(player.name()) && command.operator() == player.operator();
            case Event.PlayerJoin join -> join.uuid().equals(player.uuid().toString()) && join.name().equals(player.name());
            case Event.PlayerQuit quit -> quit.uuid().equals(player.uuid().toString()) && quit.name().equals(player.name());
            default -> false;
        };
        if (!matches || !player.online()) throw new IllegalArgumentException("Native context does not match event");
        eventPlayer = player;
    }

    NativeQueryBinding.Scope bind(String plugin) {
        if (!plugin.matches("[A-Za-z0-9_-]{1,64}")) throw new IllegalArgumentException("Invalid native plugin identity");
        return NativeQueryBinding.bind(new NativeQueries.Access() {
            @Override public NativeQueries.ServerSnapshot serverSnapshot() { return query(plugin, operation("server_snapshot"), NativeTransport::snapshot); }
            @Override public List<NativeQueries.PlayerSnapshot> onlinePlayers() {
                if (onlinePlayers == null) {
                    onlinePlayers = query(plugin, operation("online_players"), NativeTransport::playerList);
                    for (NativeQueries.PlayerSnapshot player : onlinePlayers) {
                        Optional<NativeQueries.PlayerSnapshot> snapshot = Optional.of(player);
                        players.putIfAbsent(player.uuid() + ":null", snapshot);
                        players.putIfAbsent(player.uuid() + ":" + player.sessionId(), snapshot);
                    }
                }
                return onlinePlayers;
            }
            @Override public Optional<NativeQueries.PlayerSnapshot> player(UUID uuid, Integer sessionId) {
                if (eventPlayer != null && eventPlayer.uuid().equals(uuid) && (sessionId == null || sessionId == eventPlayer.sessionId())) return Optional.of(eventPlayer);
                String key = uuid + ":" + sessionId;
                return players.computeIfAbsent(key, ignored -> {
                    JsonObject request = operation("player");
                    request.addProperty("uuid", uuid.toString());
                    if (sessionId != null) request.addProperty("session_id", sessionId);
                    return query(plugin, request, value -> {
                        Wire.fields(value, "player");
                        if (!value.has("player")) throw new IllegalArgumentException("Missing native player result");
                        if (value.get("player").isJsonNull()) return Optional.empty();
                        NativeQueries.PlayerSnapshot result = NativeTransport.player(Wire.object(value, "player"));
                        if (!result.uuid().equals(uuid) || sessionId != null && result.sessionId() != sessionId || !result.online()) throw new IllegalArgumentException("Native player result identity mismatch");
                        return Optional.of(result);
                    });
                });
            }
            @Override public List<NativeQueries.WorldSnapshot> worlds() {
                if (worlds == null) worlds = query(plugin, operation("worlds"), NativeTransport::worldList);
                return worlds;
            }
        });
    }

    void checkHealthy() throws IOException {
        if (poisoned) throw new IOException("Native query protocol is no longer synchronized");
    }

    private <T> T query(String plugin, JsonObject operation, Function<JsonObject, T> decode) {
        if (Thread.currentThread() != owner || !active) throw new IllegalStateException("Native queries require the active host callback thread");
        if (!negotiated) throw new NativeQueries.NativeQueryException(NativeQueries.ErrorCode.UNSUPPORTED, "Native queries require host protocol 2");
        if (poisoned) throw failure("Native query protocol is no longer synchronized", null);
        if (calls == MAX_CALLS) throw failure("Native query limit exceeded", null);
        long call = ++calls;
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
                return decode.apply(Wire.object(result, "value"));
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

    private static JsonObject operation(String name) {
        JsonObject operation = new JsonObject();
        operation.addProperty("query", name);
        return operation;
    }

    private static NativeQueries.ServerSnapshot snapshot(JsonObject value) {
        Wire.fields(value, "server_version", "minecraft_version", "protocol", "max_players", "online_players", "tick");
        return new NativeQueries.ServerSnapshot(Wire.string(value, "server_version", 128), Wire.string(value, "minecraft_version", 128),
                (int) Wire.integer(value, "protocol", 0, Integer.MAX_VALUE), (int) Wire.integer(value, "max_players", 1, 1_000_000),
                (int) Wire.integer(value, "online_players", 0, 1_000_000), Wire.integer(value, "tick", 0, Long.MAX_VALUE));
    }

    private static UUID uuid(JsonObject value, String field) {
        String text = Wire.string(value, field, 36);
        UUID uuid = UUID.fromString(text);
        if (!uuid.toString().equals(text)) throw new IllegalArgumentException("Noncanonical native UUID");
        return uuid;
    }

    private static double decimal(JsonObject value, String field) {
        JsonElement number = value.get(field);
        if (number == null || !number.isJsonPrimitive() || !number.getAsJsonPrimitive().isNumber()) throw new IllegalArgumentException("Missing number: " + field);
        double result = number.getAsDouble();
        if (!Double.isFinite(result)) throw new IllegalArgumentException("Invalid native number");
        return result;
    }

    private static NativeQueries.Position position(JsonObject value) {
        Wire.fields(value, "x", "y", "z", "yaw", "pitch");
        return new NativeQueries.Position(decimal(value, "x"), decimal(value, "y"), decimal(value, "z"), (float) decimal(value, "yaw"), (float) decimal(value, "pitch"));
    }

    private static NativeQueries.PlayerSnapshot player(JsonObject value) {
        Wire.fields(value, "uuid", "session_id", "entity_id", "name", "world_uuid", "position", "operator", "online");
        return new NativeQueries.PlayerSnapshot(uuid(value, "uuid"), (int) Wire.integer(value, "session_id", 1, Integer.MAX_VALUE),
                (int) Wire.integer(value, "entity_id", 1, Integer.MAX_VALUE), Wire.string(value, "name", 16), uuid(value, "world_uuid"),
                position(Wire.object(value, "position")), Wire.bool(value, "operator"), Wire.bool(value, "online"));
    }

    private static List<NativeQueries.PlayerSnapshot> playerList(JsonObject value) {
        Wire.fields(value, "players");
        JsonArray array = Wire.array(value, "players", 1024);
        List<NativeQueries.PlayerSnapshot> result = new ArrayList<>(array.size());
        HashSet<UUID> identities = new HashSet<>();
        HashSet<Integer> sessions = new HashSet<>();
        HashSet<Integer> entities = new HashSet<>();
        for (JsonElement element : array) {
            NativeQueries.PlayerSnapshot player = player(element.getAsJsonObject());
            if (!player.online() || !identities.add(player.uuid()) || !sessions.add(player.sessionId()) || !entities.add(player.entityId())) throw new IllegalArgumentException("Duplicate or offline native player");
            result.add(player);
        }
        return List.copyOf(result);
    }

    private static List<NativeQueries.WorldSnapshot> worldList(JsonObject value) {
        Wire.fields(value, "worlds");
        JsonArray array = Wire.array(value, "worlds", 16);
        List<NativeQueries.WorldSnapshot> result = new ArrayList<>(array.size());
        HashSet<UUID> identities = new HashSet<>();
        HashSet<String> keys = new HashSet<>();
        for (JsonElement element : array) {
            JsonObject world = element.getAsJsonObject();
            Wire.fields(world, "uuid", "name", "key", "dimension", "environment", "min_height", "max_height", "spawn");
            NativeQueries.WorldSnapshot snapshot = new NativeQueries.WorldSnapshot(uuid(world, "uuid"), Wire.string(world, "name", 128),
                    Wire.string(world, "key", 256), Wire.string(world, "dimension", 256), Wire.string(world, "environment", 16),
                    Wire.signedInteger(world, "min_height", -20_000_000, 20_000_000), Wire.signedInteger(world, "max_height", -20_000_000, 20_000_000), position(Wire.object(world, "spawn")));
            if (!identities.add(snapshot.uuid()) || !keys.add(snapshot.key())) throw new IllegalArgumentException("Duplicate native world");
            result.add(snapshot);
        }
        return List.copyOf(result);
    }

    private IllegalStateException failure(String message, Throwable cause) {
        poisoned = true;
        return new IllegalStateException(message, cause);
    }
}
