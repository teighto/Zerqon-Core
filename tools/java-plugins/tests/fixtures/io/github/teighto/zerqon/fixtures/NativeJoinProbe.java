package io.github.teighto.zerqon.fixtures;

import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.NativeQueries;
import io.github.teighto.zerqon.api.Plugin;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.UUID;

public final class NativeJoinProbe implements Plugin {
    private int joins;
    private int quits;
    private UUID playerId;
    private UUID worldId;
    private int sessionId;

    private Path directory() throws Exception {
        return Path.of(getClass().getProtectionDomain().getCodeSource().getLocation().toURI()).getParent();
    }

    private void counts() throws Exception {
        Files.writeString(directory().resolve("native-join-counts"), joins + ":" + quits);
    }

    @Override public List<Action> onEvent(Event event) throws Exception {
        if (event instanceof Event.Enable) {
            counts();
        } else if (event instanceof Event.PlayerJoin join) {
            NativeQueries.PlayerSnapshot player = NativeQueries.player(UUID.fromString(join.uuid())).orElseThrow();
            if (!player.name().equals(join.name()) || !player.online()) {
                throw new IllegalStateException("Join player context does not match the session");
            }
            NativeQueries.WorldSnapshot world = NativeQueries.worlds().stream()
                    .filter(candidate -> candidate.uuid().equals(player.worldUuid())).findFirst().orElseThrow();
            playerId = player.uuid();
            worldId = world.uuid();
            sessionId = player.sessionId();
            joins++;
            Files.writeString(directory().resolve("native-join-identity"), playerId + ":" + sessionId + ":" + worldId);
            counts();
        } else if (event instanceof Event.PlayerQuit quit) {
            NativeQueries.PlayerSnapshot player = NativeQueries.player(UUID.fromString(quit.uuid())).orElseThrow();
            if (!player.uuid().equals(playerId) || player.sessionId() != sessionId || !player.worldUuid().equals(worldId)) {
                throw new IllegalStateException("Quit does not belong to the joined session");
            }
            quits++;
            counts();
        } else if (event instanceof Event.Disable) {
            if (joins != 1 || quits != 1) {
                throw new IllegalStateException("unpaired Join/Quit: " + joins + ":" + quits);
            }
            Files.writeString(directory().resolve("native-join-disabled"), "disabled");
        }
        return List.of();
    }
}
