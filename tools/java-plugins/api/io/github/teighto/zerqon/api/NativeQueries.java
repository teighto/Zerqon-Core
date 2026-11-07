package io.github.teighto.zerqon.api;

import java.nio.charset.StandardCharsets;
import java.util.Objects;
import java.util.List;
import java.util.Optional;
import java.util.UUID;

public final class NativeQueries {
    static final ThreadLocal<Access> CURRENT = new ThreadLocal<>();

    private NativeQueries() {}

    public static ServerSnapshot serverSnapshot() {
        return access().serverSnapshot();
    }

    public static List<PlayerSnapshot> onlinePlayers() { return access().onlinePlayers(); }
    public static Optional<PlayerSnapshot> player(UUID uuid) { identity(uuid); return access().player(uuid, null); }
    public static Optional<PlayerSnapshot> player(UUID uuid, int sessionId) {
        if (sessionId <= 0) throw new IllegalArgumentException("Invalid player session");
        identity(uuid);
        return access().player(uuid, sessionId);
    }
    public static List<WorldSnapshot> worlds() { return access().worlds(); }

    private static Access access() {
        Access access = CURRENT.get();
        if (access == null) throw new IllegalStateException("Native queries require an active synchronous plugin callback");
        return access;
    }

    public interface Access {
        ServerSnapshot serverSnapshot();
        List<PlayerSnapshot> onlinePlayers();
        Optional<PlayerSnapshot> player(UUID uuid, Integer sessionId);
        List<WorldSnapshot> worlds();
    }

    public record Position(double x, double y, double z, float yaw, float pitch) {
        public Position {
            if (!Double.isFinite(x) || !Double.isFinite(y) || !Double.isFinite(z) || !Float.isFinite(yaw) || !Float.isFinite(pitch)
                    || Math.abs(x) > 30_000_000 || Math.abs(z) > 30_000_000 || Math.abs(y) > 20_000_000) throw new IllegalArgumentException("Invalid native position");
        }
    }

    public record PlayerSnapshot(UUID uuid, int sessionId, int entityId, String name, UUID worldUuid, Position position, boolean operator, boolean online) {
        public PlayerSnapshot {
            identity(uuid); identity(worldUuid);
            Objects.requireNonNull(position, "position");
            if (sessionId <= 0 || entityId <= 0 || name == null || !name.matches("[A-Za-z0-9_]{1,16}")) throw new IllegalArgumentException("Invalid native player");
        }
    }

    public record WorldSnapshot(UUID uuid, String name, String key, String dimension, String environment, int minHeight, int maxHeight, Position spawn) {
        public WorldSnapshot {
            identity(uuid); text(name, 128); identifier(key); identifier(dimension);
            Objects.requireNonNull(spawn, "spawn");
            if (!List.of("normal", "nether", "the_end", "custom").contains(environment)
                    || minHeight >= maxHeight || minHeight < -20_000_000 || maxHeight > 20_000_000 || minHeight % 16 != 0 || maxHeight % 16 != 0) {
                throw new IllegalArgumentException("Invalid native world");
            }
        }
    }

    private static void identity(UUID value) {
        Objects.requireNonNull(value, "uuid");
        if (value.getMostSignificantBits() == 0 && value.getLeastSignificantBits() == 0) throw new IllegalArgumentException("Nil native identity");
    }

    private static void identifier(String value) {
        if (value == null || value.length() > 256 || !value.matches("[a-z0-9_.-]+:[a-z0-9/._-]+")) throw new IllegalArgumentException("Invalid native key");
    }

    private static void text(String value, int limit) {
        Objects.requireNonNull(value, "text");
        if (value.isEmpty() || value.getBytes(StandardCharsets.UTF_8).length > limit || value.codePoints().anyMatch(Character::isISOControl)) throw new IllegalArgumentException("Invalid native text");
        for (int index = 0; index < value.length(); index++) {
            char current = value.charAt(index);
            if (Character.isHighSurrogate(current)) {
                if (++index == value.length() || !Character.isLowSurrogate(value.charAt(index))) throw new IllegalArgumentException("Invalid native text Unicode");
            } else if (Character.isLowSurrogate(current)) throw new IllegalArgumentException("Invalid native text Unicode");
        }
    }

    public record ServerSnapshot(String serverVersion, String minecraftVersion, int protocol, int maxPlayers, int onlinePlayers, long tick) {
        public ServerSnapshot {
            version(serverVersion);
            version(minecraftVersion);
            if (protocol < 0 || maxPlayers < 1 || maxPlayers > 1_000_000 || onlinePlayers < 0 || onlinePlayers > maxPlayers || tick < 0) {
                throw new IllegalArgumentException("Invalid native server snapshot");
            }
        }

        private static void version(String value) {
            text(value, 128);
        }
    }

    public enum ErrorCode {
        INVALID_ARGUMENT, NOT_FOUND, STALE_SESSION, UNAVAILABLE, UNSUPPORTED, CAPACITY, CANCELLED, DEADLINE_EXCEEDED, INTERNAL
    }

    public static final class NativeQueryException extends RuntimeException {
        private static final long serialVersionUID = 1L;
        private final ErrorCode code;

        public NativeQueryException(ErrorCode code, String message) {
            super(Objects.requireNonNull(message, "message"));
            this.code = Objects.requireNonNull(code, "code");
        }

        public ErrorCode code() { return code; }
    }
}
