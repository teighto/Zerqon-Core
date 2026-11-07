package io.github.teighto.zerqon.paper;

import java.nio.file.Path;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.UUID;
import org.bukkit.Bukkit;
import org.bukkit.entity.Player;

public final class PaperReadModelTest {
    private static int checks;
    private PaperReadModelTest() {}

    public static void main(String[] args) throws Exception {
        UUID worldId = new UUID(2, 1);
        var spawn = new ReadOnlyServerView.Position(0.5, 64, 0.5, 0, 0);
        var world = new ReadOnlyServerView.WorldData(worldId, "world", "minecraft:overworld", "minecraft:overworld", "normal", -64, 320, spawn);
        Map<UUID, ReadOnlyServerView.PlayerData> online = new LinkedHashMap<>();
        ReadOnlyServerView view = new ReadOnlyServerView() {
            @Override public List<PlayerData> onlinePlayers() { return List.copyOf(online.values()); }
            @Override public Optional<PlayerData> player(UUID uuid, Integer session) { return Optional.ofNullable(online.get(uuid)).filter(player -> session == null || session == player.sessionId()); }
            @Override public List<WorldData> worlds() { return List.of(world); }
            @Override public PlayerData eventPlayer() { return null; }
        };
        try (PaperBridge bridge = new PaperBridge(Path.of(args[0]), plugin -> () -> {}, view)) {
            UUID firstId = new UUID(1, 1);
            online.put(firstId, player(firstId, 1, worldId, 1.5));
            Player first = java.util.Objects.requireNonNull(Bukkit.getPlayer(firstId));
            check(Bukkit.getPlayer(firstId) == first, "live player proxy is reused");
            check(first.getPlayer() == first && first.hashCode() == 485 + firstId.hashCode(), "Player UUID identity contract");
            check(first.getWorld() == Bukkit.getWorld(worldId), "world proxy is reused");
            var location = first.getLocation(); location.setX(500);
            check(first.getLocation().getX() == 1.5, "Location copies do not mutate native state");
            var playerView = Bukkit.getOnlinePlayers();
            boolean immutable = false;
            try { playerView.clear(); } catch (UnsupportedOperationException expected) { immutable = true; }
            check(immutable, "online-player view is unmodifiable");
            online.clear();
            check(playerView.isEmpty(), "retained online-player view reflects current native presence");
            UUID closingId = new UUID(1, 99);
            var closingData = player(closingId, 99, worldId, 4.5);
            online.put(closingId, closingData);
            Player closing = java.util.Objects.requireNonNull(Bukkit.getPlayer(closingId));
            bridge.playerQuit(closingData);
            check(Bukkit.getPlayer(closingId) == closing && !closing.isValid(), "closing session keeps retired proxy identity until native registry removal");
            online.clear(); online.put(firstId, player(firstId, 1, worldId, 1.5));
            for (int id = 2; id <= 4096; id++) {
                UUID uuid = new UUID(1, id);
                var data = player(uuid, id, worldId, id);
                online.put(uuid, data);
                java.util.Objects.requireNonNull(Bukkit.getPlayer(uuid));
                bridge.playerQuit(data); online.remove(uuid);
            }
            check(Bukkit.getPlayer(firstId) == first, "active AFK player identity survives retired-session churn");
            var modelField = PaperBridge.class.getDeclaredField("readModel"); modelField.setAccessible(true);
            var playersField = BridgeReadModel.class.getDeclaredField("players"); playersField.setAccessible(true);
            check(((Map<?, ?>) playersField.get(modelField.get(bridge))).size() <= 2048, "retained proxy index stays bounded");
            online.clear();
            check(!first.isOnline() && !first.isValid(), "offline retained proxy reports current presence");
            check(first.getLocation().getX() == 1.5, "retired entity keeps its last observed position");
            online.put(firstId, player(firstId, 5000, worldId, 20.5));
            Player replacement = java.util.Objects.requireNonNull(Bukkit.getPlayer(firstId));
            check(first != replacement && first.equals(replacement), "new session has distinct object and equal UUID identity");
            check(first.isOnline() && !first.isValid() && first.getEntityId() == 1 && first.getLocation().getX() == 1.5, "old entity does not retarget the reconnected session");
            boolean unsupported = false;
            try { replacement.teleport(location); } catch (UnsupportedOperationException expected) { unsupported = true; }
            check(unsupported, "teleport remains unsupported");
            boolean capacity = false;
            for (int id = 6000; id < 9000; id++) {
                UUID uuid = new UUID(1, id);
                online.put(uuid, player(uuid, id, worldId, id));
                try { Bukkit.getPlayer(uuid); }
                catch (IllegalStateException failure) { capacity = failure.getMessage().contains("capacity"); break; }
            }
            check(capacity && Bukkit.getPlayer(firstId) == replacement, "full active cache fails without replacing live identities");
        }
        System.out.println("Paper read model checks: " + checks + " passed");
    }

    private static ReadOnlyServerView.PlayerData player(UUID uuid, int session, UUID world, double x) {
        return new ReadOnlyServerView.PlayerData(uuid, session, session, "User" + session, world, new ReadOnlyServerView.Position(x, 64, 0, 0, 0), false, true);
    }
    private static void check(boolean result, String message) {
        if (!result) throw new AssertionError(message);
        checks++;
    }
}
