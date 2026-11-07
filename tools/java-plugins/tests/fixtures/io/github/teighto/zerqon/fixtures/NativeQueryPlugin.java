package io.github.teighto.zerqon.fixtures;

import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.NativeQueries;
import io.github.teighto.zerqon.api.Plugin;
import java.util.List;
import java.util.concurrent.atomic.AtomicReference;

public final class NativeQueryPlugin implements Plugin {
    public NativeQueryPlugin() {
        try {
            NativeQueries.serverSnapshot();
            throw new AssertionError("Native query available outside a callback");
        } catch (IllegalStateException expected) {
            if (!expected.getMessage().contains("active synchronous")) throw expected;
        }
    }

    @Override public List<Action> onEvent(Event event) throws Exception {
        if (event instanceof Event.Enable) return List.of(new Action.RegisterCommand("native", "Native query probe", null));
        if (!(event instanceof Event.Command command)) return List.of();
        if (command.command().equals("thread")) {
            AtomicReference<Throwable> result = new AtomicReference<>();
            Thread thread = Thread.ofVirtual().start(() -> {
                try { NativeQueries.serverSnapshot(); } catch (Throwable failure) { result.set(failure); }
            });
            thread.join();
            if (!(result.get() instanceof IllegalStateException)) throw new AssertionError("Native query escaped the callback thread");
            return List.of(new Action.Reply("thread rejected"));
        }
        if (command.command().equals("catch_protocol")) {
            try { NativeQueries.serverSnapshot(); }
            catch (IllegalStateException ignored) { return List.of(new Action.Reply("caught protocol failure")); }
        }
        int calls = command.command().equals("limit") ? 129 : command.command().equals("twice") ? 2 : command.command().equals("aggregate") ? 5 : 1;
        String result = "";
        for (int index = 0; index < calls; index++) {
            try {
                NativeQueries.ServerSnapshot snapshot = NativeQueries.serverSnapshot();
                result = snapshot.serverVersion() + ":" + snapshot.minecraftVersion() + ":" + snapshot.protocol() + ":" + snapshot.maxPlayers() + ":" + snapshot.onlinePlayers() + ":" + snapshot.tick();
            } catch (NativeQueries.NativeQueryException failure) {
                result = failure.code().name();
            }
        }
        return List.of(new Action.Reply(result));
    }
}
