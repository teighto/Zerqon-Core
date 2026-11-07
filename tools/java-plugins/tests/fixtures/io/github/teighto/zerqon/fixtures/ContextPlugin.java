package io.github.teighto.zerqon.fixtures;

import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.Plugin;
import java.util.List;

public final class ContextPlugin implements Plugin {
    public ContextPlugin() {
        if (Thread.currentThread().getContextClassLoader() != getClass().getClassLoader()) {
            throw new IllegalStateException("constructor has wrong context classloader");
        }
    }

    @Override
    public List<Action> onEvent(Event event) {
        if (Thread.currentThread().getContextClassLoader() != getClass().getClassLoader()) {
            throw new IllegalStateException("callback has wrong context classloader");
        }
        return List.of();
    }
}
