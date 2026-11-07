package io.github.teighto.zerqon.paper;

import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.ArrayList;
import java.util.AbstractCollection;
import java.util.Collection;
import java.util.Collections;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Optional;
import java.util.UUID;
import org.bukkit.Location;
import org.bukkit.NamespacedKey;
import org.bukkit.OfflinePlayer;
import org.bukkit.World;
import org.bukkit.entity.EntityType;
import org.bukkit.entity.Player;
import org.bukkit.permissions.Permission;

final class BridgeReadModel {
    private record PlayerKey(UUID uuid, int session) {}
    private final PaperBridge bridge;
    private final ReadOnlyServerView view;
    private final Map<PlayerKey, PlayerState> players = new LinkedHashMap<>(16, 0.75f, true);
    private final Map<UUID, WorldState> worlds = new LinkedHashMap<>();
    private final Collection<Player> onlineView = Collections.unmodifiableCollection(new AbstractCollection<>() {
        @Override public Iterator<Player> iterator() {
            bridge.checkThread();
            return view.onlinePlayers().stream().map(BridgeReadModel.this::player).toList().iterator();
        }
        @Override public int size() { bridge.checkThread(); return view.onlinePlayers().size(); }
    });

    BridgeReadModel(PaperBridge bridge, ReadOnlyServerView view) { this.bridge = bridge; this.view = view; }

    Collection<Player> onlinePlayers() { return onlineView; }
    Player player(UUID uuid) {
        java.util.Objects.requireNonNull(uuid);
        if (uuid.getMostSignificantBits() == 0 && uuid.getLeastSignificantBits() == 0) return null;
        return view.player(uuid, null).map(this::player).orElse(null);
    }
    Player player(String name, boolean exact) {
        if (name == null) throw new IllegalArgumentException("name cannot be null");
        Player found = null;
        int delta = Integer.MAX_VALUE;
        for (Player player : onlinePlayers()) {
            if (player.getName().equalsIgnoreCase(name)) return player;
            if (!exact && player.getName().toLowerCase(Locale.ROOT).startsWith(name.toLowerCase(Locale.ROOT))) {
                int difference = player.getName().length() - name.length();
                if (difference < delta) { found = player; delta = difference; }
            }
        }
        return found;
    }

    Player player(ReadOnlyServerView.PlayerData data) {
        PlayerKey key = new PlayerKey(data.uuid(), data.sessionId());
        PlayerState state = players.get(key);
        if (state == null) {
            if (players.size() >= 2048) {
                Iterator<PlayerState> entries = players.values().iterator();
                while (entries.hasNext()) {
                    if (entries.next().retired) { entries.remove(); break; }
                }
                if (players.size() >= 2048) throw new IllegalStateException("Player proxy capacity exceeded");
            }
            state = new PlayerState(data);
            players.put(key, state);
        } else { state.last = data; }
        return state.proxy;
    }

    void quit(ReadOnlyServerView.PlayerData data) {
        PlayerState player = players.get(new PlayerKey(data.uuid(), data.sessionId()));
        if (player != null) { player.last = data; player.retired = true; }
    }

    List<World> worlds() {
        List<ReadOnlyServerView.WorldData> snapshots = view.worlds();
        List<UUID> active = snapshots.stream().map(ReadOnlyServerView.WorldData::uuid).toList();
        worlds.keySet().removeIf(uuid -> !active.contains(uuid));
        List<World> result = new ArrayList<>(snapshots.size());
        for (ReadOnlyServerView.WorldData data : snapshots) {
            WorldState state = worlds.get(data.uuid());
            if (state == null) {
                if (worlds.size() >= 16) throw new IllegalStateException("World proxy limit exceeded");
                state = new WorldState(data); worlds.put(data.uuid(), state);
            } else { state.data = data; }
            result.add(state.proxy);
        }
        return List.copyOf(result);
    }

    World world(Object identity) {
        java.util.Objects.requireNonNull(identity, "world identity");
        for (World world : worlds()) {
            if (identity instanceof UUID uuid ? world.getUID().equals(uuid)
                    : identity instanceof String name ? world.getName().equalsIgnoreCase(name)
                    : identity instanceof net.kyori.adventure.key.Key key && world.getKey().asString().equals(key.asString())) return world;
        }
        return null;
    }

    private World requiredWorld(UUID uuid) {
        World world = world(uuid);
        if (world == null) throw new IllegalStateException("Native player world is unavailable");
        return world;
    }

    private Location location(World world, ReadOnlyServerView.Position position, Object[] args) {
        if (args.length == 1 && args[0] == null) return null;
        Location location = args.length == 1 ? (Location) args[0] : new Location(world, 0, 0, 0);
        location.setWorld(world); location.setX(position.x()); location.setY(position.y()); location.setZ(position.z());
        location.setYaw(position.yaw()); location.setPitch(position.pitch());
        return location;
    }

    private final class PlayerState implements InvocationHandler {
        private ReadOnlyServerView.PlayerData last;
        private boolean retired;
        private final Player proxy;
        PlayerState(ReadOnlyServerView.PlayerData data) {
            last = data;
            proxy = (Player) Proxy.newProxyInstance(Player.class.getClassLoader(), new Class<?>[]{Player.class}, this);
        }
        private ReadOnlyServerView.PlayerData current() {
            ReadOnlyServerView.PlayerData event = view.eventPlayer();
            if (event != null && event.uuid().equals(last.uuid()) && event.sessionId() == last.sessionId()) last = event;
            else if (!retired) {
                Optional<ReadOnlyServerView.PlayerData> current = view.player(last.uuid(), last.sessionId());
                if (current.isPresent()) last = current.get(); else retired = true;
            }
            return last;
        }
        private boolean operator() {
            return view.player(last.uuid(), null).map(ReadOnlyServerView.PlayerData::operator)
                    .orElseThrow(() -> new UnsupportedOperationException("Zerqon Paper API is not implemented: offline player permissions"));
        }
        @Override public Object invoke(Object object, Method method, Object[] nullableArgs) {
            Object[] args = nullableArgs == null ? new Object[0] : nullableArgs;
            if (method.getDeclaringClass() == Object.class) {
                return switch (method.getName()) {
                    case "equals" -> args[0] == object || args[0] instanceof OfflinePlayer other && last.uuid().equals(other.getUniqueId());
                    case "hashCode" -> 485 + last.uuid().hashCode();
                    case "toString" -> "Zerqon Player " + last.uuid();
                    default -> throw PaperBridge.unsupported(method);
                };
            }
            bridge.checkThread();
            return switch (method.getName()) {
                case "getUniqueId" -> last.uuid();
                case "getEntityId" -> last.entityId();
                case "getName" -> last.name();
                case "getServer" -> bridge.server();
                case "getPlayer" -> proxy;
                case "getType" -> EntityType.PLAYER;
                case "isOnline" -> view.player(last.uuid(), null).isPresent();
                case "isValid" -> { current(); yield !retired && last.online(); }
                case "getWorld" -> requiredWorld(current().worldUuid());
                case "getLocation" -> {
                    if (args.length == 1 && args[0] == null) yield null;
                    ReadOnlyServerView.PlayerData data = current(); yield location(requiredWorld(data.worldUuid()), data.position(), args);
                }
                case "isOp" -> operator();
                case "hasPermission" -> bridge.hasPermission(args[0] instanceof Permission permission ? permission.getName() : (String) args[0], operator());
                case "isPermissionSet" -> bridge.permissionSet(args[0] instanceof Permission permission ? permission.getName() : (String) args[0]);
                case "sendMessage", "sendPlainMessage" -> { bridge.playerReply(last.uuid(), last.sessionId(), method, args); yield null; }
                default -> throw PaperBridge.unsupported(method);
            };
        }
    }

    private final class WorldState implements InvocationHandler {
        private ReadOnlyServerView.WorldData data;
        private final World proxy;
        WorldState(ReadOnlyServerView.WorldData data) {
            this.data = data;
            proxy = (World) Proxy.newProxyInstance(World.class.getClassLoader(), new Class<?>[]{World.class}, this);
        }
        @Override public Object invoke(Object object, Method method, Object[] nullableArgs) {
            Object[] args = nullableArgs == null ? new Object[0] : nullableArgs;
            if (method.getDeclaringClass() == Object.class) {
                return switch (method.getName()) {
                    case "equals" -> args[0] == object || args[0] instanceof World other && data.uuid().equals(other.getUID());
                    case "hashCode" -> data.uuid().hashCode();
                    case "toString" -> "Zerqon World " + data.name();
                    default -> throw PaperBridge.unsupported(method);
                };
            }
            bridge.checkThread();
            return switch (method.getName()) {
                case "getUID" -> data.uuid();
                case "getName" -> data.name();
                case "getKey", "key" -> java.util.Objects.requireNonNull(NamespacedKey.fromString(data.key()));
                case "getMinHeight" -> data.minHeight();
                case "getMaxHeight" -> data.maxHeight();
                case "getEnvironment" -> World.Environment.valueOf(data.environment().toUpperCase(Locale.ROOT));
                case "getSpawnLocation" -> location(proxy, data.spawn(), args);
                case "getPlayers" -> view.onlinePlayers().stream().filter(player -> player.worldUuid().equals(data.uuid())).map(BridgeReadModel.this::player).toList();
                case "getPlayerCount" -> (int) view.onlinePlayers().stream().filter(player -> player.worldUuid().equals(data.uuid())).count();
                default -> throw PaperBridge.unsupported(method);
            };
        }
    }
}
