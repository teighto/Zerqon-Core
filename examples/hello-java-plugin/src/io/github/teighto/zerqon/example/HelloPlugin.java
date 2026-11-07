package io.github.teighto.zerqon.example;

import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.Plugin;
import java.util.List;

public final class HelloPlugin implements Plugin {
    @Override
    public List<Action> onEvent(Event event) {
        return switch (event) {
            case Event.Enable ignored -> List.of(
                    new Action.Log(Action.Level.INFO, "Hello Java plugin enabled"),
                    new Action.RegisterCommand("jhello", "A greeting from a Java plugin", null));
            case Event.Command command when command.command().equals("jhello") -> List.of(
                    new Action.Reply("Hello, " + command.sender() + "! This plugin uses the Zerqon Java API."));
            case Event.Disable ignored -> List.of(new Action.Log(Action.Level.INFO, "Hello Java plugin disabled"));
            default -> List.of();
        };
    }
}
