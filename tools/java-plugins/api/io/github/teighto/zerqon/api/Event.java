package io.github.teighto.zerqon.api;

import java.util.List;
import java.util.Objects;

public sealed interface Event {
    enum SenderKind { CONSOLE, PLAYER }

    record Enable(String serverVersion) implements Event {
        public Enable { Objects.requireNonNull(serverVersion, "serverVersion"); }
    }

    record Disable() implements Event {}

    record Command(String sender, SenderKind senderKind, boolean operator, String command, List<String> args) implements Event {
        public Command(String sender, SenderKind senderKind, String command, List<String> args) {
            this(sender, senderKind, false, command, args);
        }
        public Command {
            Objects.requireNonNull(sender, "sender");
            Objects.requireNonNull(senderKind, "senderKind");
            Objects.requireNonNull(command, "command");
            args = List.copyOf(args);
        }
    }

    record Tick(long tick) implements Event {
        public Tick { if (tick < 0) throw new IllegalArgumentException("tick must not be negative"); }
    }

    record PlayerJoin(String uuid, String name) implements Event {
        public PlayerJoin {
            Objects.requireNonNull(uuid, "uuid");
            Objects.requireNonNull(name, "name");
        }
    }

    record PlayerQuit(String uuid, String name) implements Event {
        public PlayerQuit {
            Objects.requireNonNull(uuid, "uuid");
            Objects.requireNonNull(name, "name");
        }
    }
}
