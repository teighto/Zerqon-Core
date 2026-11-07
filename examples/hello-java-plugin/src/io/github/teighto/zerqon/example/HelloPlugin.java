package io.github.teighto.zerqon.example;

import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.NativeQueries;
import io.github.teighto.zerqon.api.Plugin;
import java.util.List;

public final class HelloPlugin implements Plugin {
    @Override
    public List<Action> onEvent(Event event) {
        return switch (event) {
            case Event.Enable ignored -> List.of(
                    new Action.Log(Action.Level.INFO, "Hello Java plugin enabled"),
                    new Action.RegisterCommand("jhello", "A greeting from a Java plugin", null),
                    new Action.RegisterCommand("jserver", "Show the native server snapshot", null));
            case Event.Command command when command.command().equals("jhello") -> List.of(
                    new Action.Reply("Hello, " + command.sender() + "! This plugin uses the Zerqon Java API."));
            case Event.Command command when command.command().equals("jserver") -> List.of(new Action.Reply(serverSnapshot()));
            case Event.Disable ignored -> List.of(new Action.Log(Action.Level.INFO, "Hello Java plugin disabled"));
            default -> List.of();
        };
    }

    private static String serverSnapshot() {
        try {
            NativeQueries.ServerSnapshot snapshot = NativeQueries.serverSnapshot();
            return "Zerqon " + snapshot.serverVersion() + " | Minecraft " + snapshot.minecraftVersion()
                    + " | Players " + snapshot.onlinePlayers() + "/" + snapshot.maxPlayers() + " | Tick " + snapshot.tick();
        } catch (NativeQueries.NativeQueryException failure) {
            if (failure.code() == NativeQueries.ErrorCode.UNSUPPORTED) return "Native server queries are unavailable (protocol 2 required)";
            return "Native server query failed: " + failure.code().name().toLowerCase(java.util.Locale.ROOT);
        }
    }
}
