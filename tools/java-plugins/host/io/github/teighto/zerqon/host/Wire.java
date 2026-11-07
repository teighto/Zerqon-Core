package io.github.teighto.zerqon.host;

import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonNull;
import com.google.gson.JsonObject;
import com.google.gson.JsonPrimitive;
import com.google.gson.Strictness;
import com.google.gson.stream.JsonReader;
import com.google.gson.stream.JsonToken;
import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import java.io.IOException;
import java.io.StringReader;
import java.math.BigDecimal;
import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.Set;

final class Wire {
    private static final BigInteger MAX_ID = new BigInteger("18446744073709551615");

    private Wire() {}

    static JsonObject parse(String line) throws IOException {
        try (JsonReader reader = new JsonReader(new StringReader(line))) {
            reader.setStrictness(Strictness.STRICT);
            reader.setNestingLimit(32);
            JsonElement result = readValue(reader);
            if (!result.isJsonObject() || reader.peek() != JsonToken.END_DOCUMENT) {
                throw new IllegalArgumentException("request must be one JSON object");
            }
            return result.getAsJsonObject();
        }
    }

    private static JsonElement readValue(JsonReader reader) throws IOException {
        return switch (reader.peek()) {
            case BEGIN_OBJECT -> {
                JsonObject object = new JsonObject();
                reader.beginObject();
                while (reader.hasNext()) {
                    String name = reader.nextName();
                    if (object.has(name)) throw new IllegalArgumentException("duplicate JSON field: " + name);
                    object.add(name, readValue(reader));
                }
                reader.endObject();
                yield object;
            }
            case BEGIN_ARRAY -> {
                JsonArray array = new JsonArray();
                reader.beginArray();
                while (reader.hasNext()) array.add(readValue(reader));
                reader.endArray();
                yield array;
            }
            case STRING -> new JsonPrimitive(reader.nextString());
            case NUMBER -> new JsonPrimitive(new BigDecimal(reader.nextString()));
            case BOOLEAN -> new JsonPrimitive(reader.nextBoolean());
            case NULL -> {
                reader.nextNull();
                yield JsonNull.INSTANCE;
            }
            default -> throw new IllegalArgumentException("invalid JSON value");
        };
    }

    static JsonElement id(JsonObject request) {
        JsonElement value = request.get("id");
        if (value == null || !value.isJsonPrimitive() || !value.getAsJsonPrimitive().isNumber()
                || !value.getAsString().matches("0|[1-9][0-9]{0,19}")) {
            throw new IllegalArgumentException("id must be an unsigned 64-bit integer");
        }
        BigInteger id = value.getAsBigInteger();
        if (id.compareTo(MAX_ID) > 0) throw new IllegalArgumentException("id exceeds unsigned 64-bit range");
        return value;
    }

    static void fields(JsonObject object, String... names) {
        Set<String> allowed = Set.of(names);
        for (String name : object.keySet()) {
            if (!allowed.contains(name)) throw new IllegalArgumentException("unknown field: " + name);
        }
    }

    static String string(JsonObject object, String field, int maxBytes) {
        JsonElement value = object.get(field);
        if (value == null || !value.isJsonPrimitive() || !value.getAsJsonPrimitive().isString()) {
            throw new IllegalArgumentException("missing string: " + field);
        }
        String text = value.getAsString();
        boundedText(text, maxBytes);
        return text;
    }

    static Event event(JsonObject object) {
        String type = string(object, "event", 32);
        return switch (type) {
            case "enable" -> {
                fields(object, "event", "server_version");
                yield new Event.Enable(string(object, "server_version", 128));
            }
            case "disable" -> {
                fields(object, "event");
                yield new Event.Disable();
            }
            case "command" -> {
                fields(object, "event", "sender", "sender_kind", "operator", "command", "args");
                JsonElement argsValue = object.get("args");
                if (argsValue == null || !argsValue.isJsonArray() || argsValue.getAsJsonArray().size() > 256) {
                    throw new IllegalArgumentException("args must be an array of at most 256 strings");
                }
                List<String> args = new ArrayList<>();
                for (JsonElement arg : argsValue.getAsJsonArray()) {
                    if (!arg.isJsonPrimitive() || !arg.getAsJsonPrimitive().isString()) throw new IllegalArgumentException("argument must be a string");
                    boundedText(arg.getAsString(), 4096);
                    args.add(arg.getAsString());
                }
                Event.SenderKind senderKind = switch (string(object, "sender_kind", 16)) {
                    case "console" -> Event.SenderKind.CONSOLE;
                    case "player" -> Event.SenderKind.PLAYER;
                    default -> throw new IllegalArgumentException("sender_kind must be console or player");
                };
                JsonElement operator = object.get("operator");
                if (operator != null && (!operator.isJsonPrimitive() || !operator.getAsJsonPrimitive().isBoolean())) throw new IllegalArgumentException("operator must be a boolean");
                yield new Event.Command(string(object, "sender", 128), senderKind, operator != null && operator.getAsBoolean(), string(object, "command", 64), args);
            }
            case "tick" -> {
                fields(object, "event", "tick");
                JsonElement tick = object.get("tick");
                if (tick == null || !tick.isJsonPrimitive() || !tick.getAsJsonPrimitive().isNumber()) throw new IllegalArgumentException("tick must be an integer");
                yield new Event.Tick(tick.getAsBigDecimal().longValueExact());
            }
            case "player_join" -> {
                fields(object, "event", "uuid", "name");
                yield new Event.PlayerJoin(string(object, "uuid", 36), string(object, "name", 64));
            }
            case "player_quit" -> {
                fields(object, "event", "uuid", "name");
                yield new Event.PlayerQuit(string(object, "uuid", 36), string(object, "name", 64));
            }
            default -> throw new IllegalArgumentException("unknown event: " + type);
        };
    }

    static JsonArray actions(List<Action> actions, Event event) {
        if (actions == null || actions.size() > 32) throw new IllegalArgumentException("callback must return at most 32 actions");
        JsonArray output = new JsonArray();
        int totalBytes = 0;
        for (Action action : actions) {
            JsonObject value = new JsonObject();
            switch (action) {
                case Action.Log log -> {
                    boundedText(log.message(), 4096);
                    value.addProperty("action", "log");
                    value.addProperty("level", log.level().name().toLowerCase(Locale.ROOT));
                    value.addProperty("message", log.message());
                }
                case Action.Reply reply -> {
                    if (!(event instanceof Event.Command)) throw new IllegalArgumentException("replies require a command event");
                    boundedText(reply.message(), 4096);
                    value.addProperty("action", "reply");
                    value.addProperty("message", reply.message());
                }
                case Action.RegisterCommand command -> {
                    if (!(event instanceof Event.Enable)) throw new IllegalArgumentException("command registration requires Enable");
                    if (!command.name().matches("[a-z0-9_-]{1,64}")) throw new IllegalArgumentException("invalid command name");
                    boundedText(command.description(), 256);
                    if (command.permission() != null) {
                        boundedText(command.permission(), 128);
                        if (command.permission().isEmpty()) throw new IllegalArgumentException("empty permission");
                    }
                    value.addProperty("action", "register_command");
                    value.addProperty("name", command.name());
                    value.addProperty("description", command.description());
                    value.addProperty("permission", command.permission());
                }
            }
            totalBytes += value.toString().getBytes(StandardCharsets.UTF_8).length;
            if (totalBytes > 65536) throw new IllegalArgumentException("callback output exceeds 64 KiB");
            output.add(value);
        }
        return output;
    }

    static void boundedText(String text, int maxBytes) {
        for (int i = 0; i < text.length(); i++) {
            char current = text.charAt(i);
            if (Character.isHighSurrogate(current)) {
                if (++i == text.length() || !Character.isLowSurrogate(text.charAt(i))) throw new IllegalArgumentException("unpaired Unicode surrogate");
            } else if (Character.isLowSurrogate(current)) {
                throw new IllegalArgumentException("unpaired Unicode surrogate");
            }
        }
        if (text.getBytes(StandardCharsets.UTF_8).length > maxBytes) throw new IllegalArgumentException("text exceeds " + maxBytes + " UTF-8 bytes");
    }

    static String error(Throwable error) {
        String message = String.valueOf(error.getMessage());
        String text = error.getClass().getSimpleName() + ": " + message.substring(0, Math.min(message.length(), 1024));
        StringBuilder safe = new StringBuilder();
        int bytes = 0;
        for (int offset = 0; offset < text.length();) {
            int codePoint = text.codePointAt(offset);
            offset += Character.charCount(codePoint);
            String piece = Character.isISOControl(codePoint) || (codePoint >= 0xD800 && codePoint <= 0xDFFF)
                    ? "?" : new String(Character.toChars(codePoint));
            int size = piece.getBytes(StandardCharsets.UTF_8).length;
            if (bytes + size > 1024) break;
            safe.append(piece);
            bytes += size;
        }
        return safe.toString();
    }
}
