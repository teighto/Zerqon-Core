package io.github.teighto.zerqon.host;

import com.google.gson.JsonArray;
import com.google.gson.JsonObject;
import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.paper.PaperBridge;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import org.bukkit.event.player.PlayerJoinEvent;
import org.bukkit.event.player.PlayerQuitEvent;
import org.bukkit.plugin.PluginDescriptionFile;
import org.bukkit.plugin.java.JavaPlugin;

final class PaperSupport implements AutoCloseable {
    private final PaperBridge bridge;
    private final Map<String, Entry> entries = new LinkedHashMap<>();
    private final Map<String, String> metadataNames = new LinkedHashMap<>();
    private boolean closed;

    PaperSupport(Path directory, NativeTransport transport) {
        Path serverDirectory = directory;
        if (directory.getFileName().toString().equals("java-plugins") && directory.getParent() != null && directory.getParent().getFileName().toString().equals("plugins")) {
            serverDirectory = directory.getParent().getParent();
        }
        bridge = new PaperBridge(serverDirectory, plugin -> {
            String identity = metadataNames.get(plugin.getName().toLowerCase(Locale.ROOT));
            if (identity == null) throw new IllegalStateException("Paper callback has no host identity");
            var scope = transport.bind(identity);
            return scope::close;
        });
    }

    JsonArray load(List<Path> paths, String version) {
        JsonArray results = new JsonArray();
        Map<String, Candidate> pending = new LinkedHashMap<>();
        for (Path path : paths) {
            String filename = identity(path);
            if (entries.containsKey(filename)) { Entry existing = entries.get(filename); results.add(result(filename, existing.enabled(), new JsonArray(), "plugin already loaded")); continue; }
            try {
                PluginDescriptionFile metadata = PaperBridge.description(path);
                String key = metadata.getName().toLowerCase(Locale.ROOT);
                if (pending.containsKey(key) || metadataNames.containsKey(key)) { throw new IllegalArgumentException("Duplicate Paper plugin metadata name: " + metadata.getName()); }
                pending.put(key, new Candidate(path, filename, metadata));
            } catch (Exception | LinkageError failure) { results.add(result(filename, false, new JsonArray(), Wire.error(failure))); }
        }
        while (!pending.isEmpty()) {
            boolean progress = false;
            for (var candidate : new ArrayList<>(pending.entrySet())) {
                Candidate value = candidate.getValue();
                boolean waiting = value.metadata.getDepend().stream().map(name -> name.toLowerCase(Locale.ROOT)).anyMatch(pending::containsKey);
                if (waiting) { continue; }
                pending.remove(candidate.getKey()); progress = true;
                JavaPlugin plugin = null;
                try {
                    metadataNames.put(candidate.getKey(), value.identity);
                    plugin = bridge.load(value.path);
                    bridge.enable(plugin);
                    List<Action> actions = new ArrayList<>();
                    for (PaperBridge.CommandInfo command : bridge.commands(plugin)) {
                        String permission = command.permission();
                        actions.add(new Action.RegisterCommand(command.name(), command.description(), permission == null || permission.isEmpty() ? null : permission));
                    }
                    JsonArray output = Wire.actions(actions, new Event.Enable(version));
                    entries.put(value.identity, new Entry(plugin));
                    metadataNames.put(candidate.getKey(), value.identity);
                    results.add(result(value.identity, true, output, null));
                } catch (Exception | LinkageError | AssertionError failure) {
                    if (plugin != null && plugin.isEnabled()) { try { bridge.disable(plugin); } catch (RuntimeException cleanup) { failure.addSuppressed(cleanup); } }
                    metadataNames.remove(candidate.getKey());
                    results.add(result(value.identity, false, new JsonArray(), Wire.error(failure)));
                }
            }
            if (!progress) {
                for (Candidate candidate : pending.values()) { results.add(result(candidate.identity, false, new JsonArray(), "Circular Paper plugin dependency")); }
                break;
            }
        }
        return results;
    }

    boolean contains(String name) { return entries.containsKey(name); }
    int size() { return entries.size(); }
    JsonObject existing(String name) { Entry entry = entries.get(name); return result(name, entry.enabled(), new JsonArray(), "plugin already loaded"); }

    JsonArray list() {
        JsonArray result = new JsonArray();
        entries.forEach((name, entry) -> result.add(result(name, entry.enabled(), new JsonArray(), entry.error)));
        return result;
    }

    JsonArray dispatch(String target, Event event) {
        JsonArray results = new JsonArray();
        if (event instanceof Event.Tick tick) {
            if (target != null) { throw new IllegalArgumentException("Paper ticks must target the whole host"); }
            bridge.tick(tick.tick());
        }
        for (var entry : entries.entrySet()) {
            if (target != null && !target.equals(entry.getKey())) { continue; }
            Entry loaded = entry.getValue();
            if (!loaded.enabled()) {
                if (target != null) { results.add(result(entry.getKey(), false, new JsonArray(), loaded.error)); }
                continue;
            }
            try {
                List<Action> actions = new ArrayList<>();
                if (event instanceof Event.Command command) {
                    if (target == null) { continue; }
                    for (String reply : bridge.executeFor(loaded.plugin, command.sender(), command.operator(), command.senderKind() == Event.SenderKind.CONSOLE, command.command(), command.args())) { actions.add(new Action.Reply(reply)); }
                } else if (event instanceof Event.PlayerJoin && bridge.hasListeners(loaded.plugin, PlayerJoinEvent.class)) {
                    throw new UnsupportedOperationException("Native Paper PlayerJoinEvent adapter is not implemented");
                } else if (event instanceof Event.PlayerQuit && bridge.hasListeners(loaded.plugin, PlayerQuitEvent.class)) {
                    throw new UnsupportedOperationException("Native Paper PlayerQuitEvent adapter is not implemented");
                } else if (event instanceof Event.Disable) { bridge.disable(loaded.plugin); }
                results.add(result(entry.getKey(), loaded.enabled(), Wire.actions(actions, event), null));
            } catch (Exception | LinkageError | AssertionError failure) {
                loaded.error = Wire.error(failure);
                try { if (loaded.plugin.isEnabled()) { bridge.disable(loaded.plugin); } }
                catch (RuntimeException cleanup) { failure.addSuppressed(cleanup); }
                results.add(result(entry.getKey(), false, new JsonArray(), loaded.error));
            }
        }
        return results;
    }

    JsonArray shutdown() {
        JsonArray results = new JsonArray();
        if (closed) { return results; }
        for (var entry : new ArrayList<>(entries.entrySet()).reversed()) {
            Entry plugin = entry.getValue();
            try { if (plugin.enabled()) { bridge.disable(plugin.plugin); } }
            catch (RuntimeException | LinkageError | AssertionError failure) { plugin.error = Wire.error(failure); }
            results.add(result(entry.getKey(), false, new JsonArray(), plugin.error));
        }
        entries.clear();
        try { bridge.close(); } finally { metadataNames.clear(); closed = true; }
        return results;
    }

    @Override public void close() { shutdown(); }
    private static String identity(Path path) { String file = path.getFileName().toString(); return file.substring(0, file.length() - 4); }
    private static JsonObject result(String name, boolean enabled, JsonArray actions, String error) {
        JsonObject value = new JsonObject(); value.addProperty("name", name); value.addProperty("enabled", enabled); value.add("actions", actions);
        if (error != null) { value.addProperty("error", error); }
        return value;
    }
    private record Candidate(Path path, String identity, PluginDescriptionFile metadata) {}
    private static final class Entry {
        private final JavaPlugin plugin;
        private String error;
        Entry(JavaPlugin plugin) { this.plugin = plugin; }
        boolean enabled() { return plugin.isEnabled() && error == null; }
    }
}
