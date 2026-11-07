package io.github.teighto.zerqon.fixtures;

import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.NativeQueries;
import io.github.teighto.zerqon.api.Plugin;
import java.util.List;

public final class NativeLifecyclePlugin implements Plugin {
    @Override public List<Action> onEvent(Event event) {
        NativeQueries.ServerSnapshot snapshot = NativeQueries.serverSnapshot();
        return List.of(new Action.Log(Action.Level.INFO, Long.toString(snapshot.tick())));
    }
}
