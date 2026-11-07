package io.github.teighto.zerqon.api;

import java.util.List;

@FunctionalInterface
public interface Plugin {
    int API_VERSION = 1;

    List<Action> onEvent(Event event) throws Exception;
}
