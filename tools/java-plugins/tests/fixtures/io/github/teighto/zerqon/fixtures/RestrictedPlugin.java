package io.github.teighto.zerqon.fixtures;

import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.Plugin;
import java.util.List;

public final class RestrictedPlugin implements Plugin {
    @Override public List<Action> onEvent(Event event) {
        if (event instanceof Event.Enable) return List.of(new Action.RegisterCommand("private", "operator test", "test.private"));
        if (event instanceof Event.Command command) return List.of(new Action.Reply("allowed:" + command.operator()));
        return List.of();
    }
}
