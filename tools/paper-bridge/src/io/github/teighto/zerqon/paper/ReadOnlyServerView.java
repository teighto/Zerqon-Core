package io.github.teighto.zerqon.paper;

import java.util.List;
import java.util.Optional;
import java.util.UUID;

public interface ReadOnlyServerView {
    List<PlayerData> onlinePlayers();
    Optional<PlayerData> player(UUID uuid, Integer sessionId);
    List<WorldData> worlds();
    PlayerData eventPlayer();

    record Position(double x, double y, double z, float yaw, float pitch) {}
    record PlayerData(UUID uuid, int sessionId, int entityId, String name, UUID worldUuid, Position position, boolean operator, boolean online) {}
    record WorldData(UUID uuid, String name, String key, String dimension, String environment, int minHeight, int maxHeight, Position spawn) {}
}
