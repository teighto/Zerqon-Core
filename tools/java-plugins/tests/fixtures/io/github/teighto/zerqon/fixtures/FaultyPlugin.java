package io.github.teighto.zerqon.fixtures;

import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.Plugin;
import java.util.List;
import java.util.Collections;

public final class FaultyPlugin implements Plugin {
    @Override
    public List<Action> onEvent(Event event) {
        if (event instanceof Event.Command command) {
            return switch (command.command()) {
                case "flood" -> Collections.nCopies(33, new Action.Log(Action.Level.INFO, "flood"));
                case "long" -> List.of(new Action.Reply("x".repeat(4097)));
                case "surrogate" -> List.of(new Action.Reply("\uD800"));
                case "register" -> List.of(new Action.RegisterCommand("late", "late", null));
                default -> throw new IllegalStateException("fixture failure");
            };
        }
        System.out.println("fixture plugin stdout");
        return List.of();
    }
}
