package io.github.teighto.zerqon.fixtures;

import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.Plugin;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.concurrent.TimeUnit;

public final class JoinBarrier implements Plugin {
    private int joins;
    private int quits;

    @Override public List<Action> onEvent(Event event) throws Exception {
        Path directory = Path.of(getClass().getProtectionDomain().getCodeSource().getLocation().toURI()).getParent();
        if (event instanceof Event.PlayerJoin) {
            joins++;
            Files.writeString(directory.resolve("join-entered"), "entered");
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
            while (!Files.exists(directory.resolve("join-release"))) {
                if (System.nanoTime() >= deadline) throw new IllegalStateException("join barrier timed out");
                Thread.sleep(5);
            }
        } else if (event instanceof Event.PlayerQuit) {
            quits++;
            if (Files.exists(directory.resolve("hold-quit"))) {
                Files.writeString(directory.resolve("quit-entered"), "entered");
                long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
                while (!Files.exists(directory.resolve("quit-release"))) {
                    if (System.nanoTime() >= deadline) throw new IllegalStateException("quit barrier timed out");
                    Thread.sleep(5);
                }
            }
            Files.writeString(directory.resolve("join-counts"), joins + ":" + quits);
        } else if (event instanceof Event.Disable && joins != quits) {
            throw new IllegalStateException("unpaired join/quit: " + joins + ":" + quits);
        }
        return List.of();
    }
}
