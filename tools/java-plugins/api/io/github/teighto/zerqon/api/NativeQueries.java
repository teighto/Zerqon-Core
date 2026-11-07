package io.github.teighto.zerqon.api;

import java.nio.charset.StandardCharsets;
import java.util.Objects;
import java.util.function.Supplier;

public final class NativeQueries {
    static final ThreadLocal<Supplier<ServerSnapshot>> CURRENT = new ThreadLocal<>();

    private NativeQueries() {}

    public static ServerSnapshot serverSnapshot() {
        Supplier<ServerSnapshot> query = CURRENT.get();
        if (query == null) throw new IllegalStateException("Native queries require an active synchronous plugin callback");
        return query.get();
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
            Objects.requireNonNull(value, "version");
            if (value.isEmpty() || value.getBytes(StandardCharsets.UTF_8).length > 128 || value.codePoints().anyMatch(Character::isISOControl)) {
                throw new IllegalArgumentException("Invalid native version string");
            }
            for (int index = 0; index < value.length(); index++) {
                char current = value.charAt(index);
                if (Character.isHighSurrogate(current)) {
                    if (++index == value.length() || !Character.isLowSurrogate(value.charAt(index))) throw new IllegalArgumentException("Invalid native version Unicode");
                } else if (Character.isLowSurrogate(current)) {
                    throw new IllegalArgumentException("Invalid native version Unicode");
                }
            }
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
