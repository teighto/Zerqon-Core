package io.github.teighto.zerqon.api;

import java.util.Objects;

public sealed interface Action {
    enum Level { INFO, WARN, ERROR }

    record Log(Level level, String message) implements Action {
        public Log {
            Objects.requireNonNull(level, "level");
            Objects.requireNonNull(message, "message");
        }
    }

    record Reply(String message) implements Action {
        public Reply { Objects.requireNonNull(message, "message"); }
    }

    record RegisterCommand(String name, String description, String permission) implements Action {
        public RegisterCommand {
            Objects.requireNonNull(name, "name");
            Objects.requireNonNull(description, "description");
        }
    }
}
