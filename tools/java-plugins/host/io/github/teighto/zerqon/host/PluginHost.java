package io.github.teighto.zerqon.host;

import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonNull;
import com.google.gson.JsonObject;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.Plugin;
import java.io.BufferedInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.PrintStream;
import java.lang.reflect.InvocationTargetException;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.ByteBuffer;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.jar.JarFile;
import java.util.jar.Manifest;

public final class PluginHost implements AutoCloseable {
    private static final int MAX_PLUGINS = 64;
    private static final int MAX_LINE_BYTES = 65536;
    private final Map<String, LoadedPlugin> plugins = new TreeMap<>();
    private PaperSupport paper;

    private PluginHost() {}

    public static void main(String[] args) throws IOException {
        PrintStream protocol = new PrintStream(System.out, true, StandardCharsets.UTF_8);
        InputStream requests = new BufferedInputStream(System.in);
        System.setErr(new PrintStream(System.err, true, StandardCharsets.UTF_8));
        System.setOut(System.err);
        System.setIn(InputStream.nullInputStream());
        try (PluginHost host = new PluginHost()) {
            byte[] line;
            while ((line = readLine(requests)) != null) {
                JsonElement id = JsonNull.INSTANCE;
                JsonObject response;
                boolean shutdown = false;
                try {
                    String text = StandardCharsets.UTF_8.newDecoder()
                            .onMalformedInput(CodingErrorAction.REPORT)
                            .onUnmappableCharacter(CodingErrorAction.REPORT)
                            .decode(ByteBuffer.wrap(line)).toString();
                    JsonObject request = Wire.parse(text);
                    id = Wire.id(request);
                    String operation = Wire.string(request, "op", 32);
                    response = host.handle(id, operation, request);
                    shutdown = operation.equals("shutdown");
                } catch (RuntimeException | IOException exception) {
                    response = response(id, false, new JsonArray());
                    response.addProperty("error", Wire.error(exception));
                }
                String output = response.toString();
                if (output.getBytes(StandardCharsets.UTF_8).length > 8 * 1024 * 1024) throw new IOException("response exceeds 8 MiB");
                protocol.println(output);
                protocol.flush();
                if (protocol.checkError()) throw new IOException("protocol output closed");
                if (shutdown) break;
            }
        }
    }

    private JsonObject handle(JsonElement id, String operation, JsonObject request) throws IOException {
        JsonArray results = switch (operation) {
            case "load" -> {
                Wire.fields(request, "id", "op", "directory", "server_version");
                yield loadDirectory(Path.of(Wire.string(request, "directory", 4096)), Wire.string(request, "server_version", 128));
            }
            case "event" -> {
                Wire.fields(request, "id", "op", "plugin", "event");
                JsonElement event = request.get("event");
                if (event == null || !event.isJsonObject()) throw new IllegalArgumentException("event must be an object");
                String target = request.has("plugin") ? Wire.string(request, "plugin", 64) : null;
                yield dispatch(target, Wire.event(event.getAsJsonObject()));
            }
            case "list" -> {
                Wire.fields(request, "id", "op");
                JsonArray list = new JsonArray();
                for (LoadedPlugin plugin : plugins.values()) list.add(result(plugin.name, plugin.enabled(), new JsonArray(), plugin.error));
                if (paper != null) list.addAll(paper.list());
                yield list;
            }
            case "shutdown" -> {
                Wire.fields(request, "id", "op");
                yield shutdown();
            }
            default -> throw new IllegalArgumentException("unknown operation: " + operation);
        };
        return response(id, true, results);
    }

    private JsonArray loadDirectory(Path directory, String serverVersion) throws IOException {
        Files.createDirectories(directory);
        List<Path> jars = new ArrayList<>();
        try (var entries = Files.newDirectoryStream(directory)) {
            for (Path path : entries) {
                if (Files.isRegularFile(path, LinkOption.NOFOLLOW_LINKS) && path.getFileName().toString().endsWith(".jar")) {
                    if (!path.getFileName().toString().matches("[A-Za-z0-9_-]{1,64}\\.jar")) {
                        throw new IllegalArgumentException("invalid plugin file name; use 1..64 ASCII letters, digits, underscores or hyphens before .jar");
                    }
                    jars.add(path);
                }
                if (jars.size() > MAX_PLUGINS) throw new IOException("too many Java plugin files; maximum is 64");
            }
        }
        jars.sort(Path::compareTo);
        JsonArray results = new JsonArray();
        List<Path> paperJars = new ArrayList<>();
        for (Path path : jars) {
            String identity = path.getFileName().toString().replaceFirst("\\.jar$", "");
            LoadedPlugin existing = plugins.get(identity);
            if (existing != null) { results.add(result(identity, existing.enabled(), new JsonArray(), "plugin already loaded")); continue; }
            if (paper != null && paper.contains(identity)) { results.add(paper.existing(identity)); continue; }
            if (plugins.size() + paperJars.size() + (paper == null ? 0 : paper.size()) >= MAX_PLUGINS) { results.add(result(identity, false, new JsonArray(), "plugin count limit reached")); continue; }
            try {
                if (Files.size(path) > 16 * 1024 * 1024) throw new IOException("plugin JAR exceeds 16 MiB");
                boolean paperPlugin;
                try (JarFile jar = new JarFile(path.toFile())) { paperPlugin = jar.getJarEntry("plugin.yml") != null || jar.getJarEntry("paper-plugin.yml") != null; }
                if (paperPlugin) paperJars.add(path); else results.add(load(path, serverVersion));
            } catch (IOException failure) {
                String name = path.getFileName().toString().replaceFirst("\\.jar$", "");
                results.add(result(name, false, new JsonArray(), Wire.error(failure)));
            }
        }
        if (!paperJars.isEmpty()) {
            try {
                if (paper == null) paper = new PaperSupport(directory);
                results.addAll(paper.load(paperJars, serverVersion));
            } catch (RuntimeException | LinkageError failure) {
                for (Path path : paperJars) results.add(result(path.getFileName().toString().replaceFirst("\\.jar$", ""), false, new JsonArray(), "Paper bridge unavailable: " + Wire.error(failure)));
            }
        }
        return results;
    }

    private JsonObject load(Path path, String serverVersion) {
        String fileName = path.getFileName().toString();
        String name = fileName.substring(0, fileName.length() - 4);
        LoadedPlugin existing = plugins.get(name);
        if (existing != null) return result(name, existing.enabled(), new JsonArray(), "plugin already loaded");
        URLClassLoader loader = null;
        try {
            if (!name.matches("[A-Za-z0-9_-]{1,64}")) throw new IllegalArgumentException("invalid plugin file name");
            if (plugins.size() >= MAX_PLUGINS) throw new IllegalArgumentException("plugin count limit reached");
            if (Files.size(path) > 16 * 1024 * 1024) throw new IOException("plugin JAR exceeds 16 MiB");
            String entry;
            try (JarFile jar = new JarFile(path.toFile(), true)) {
                Manifest manifest = jar.getManifest();
                if (manifest == null || (entry = manifest.getMainAttributes().getValue("Zerqon-Plugin")) == null) {
                    throw new IllegalArgumentException("missing Zerqon-Plugin manifest entry; this host accepts the Zerqon API only");
                }
                if (!"1".equals(manifest.getMainAttributes().getValue("Zerqon-API"))) throw new IllegalArgumentException("unsupported Zerqon-API; expected 1");
                if (!entry.matches("[A-Za-z_$][A-Za-z0-9_$.]{0,255}")) throw new IllegalArgumentException("invalid plugin entry class");
            }
            loader = new URLClassLoader("zerqon-" + name, new URL[] { path.toUri().toURL() }, Plugin.class.getClassLoader());
            Plugin instance = construct(entry, loader);
            LoadedPlugin loaded = new LoadedPlugin(name, loader, instance);
            JsonObject result = invoke(loaded, new Event.Enable(serverVersion));
            if (loaded.enabled()) plugins.put(name, loaded);
            return result;
        } catch (Exception | LinkageError | AssertionError error) {
            if (loader != null) closeLoader(loader, error);
            Throwable cause = error instanceof InvocationTargetException target && target.getCause() != null ? target.getCause() : error;
            return result(name, false, new JsonArray(), Wire.error(cause));
        }
    }

    private static Plugin construct(String entry, URLClassLoader loader) throws ReflectiveOperationException {
        Thread thread = Thread.currentThread();
        ClassLoader previous = thread.getContextClassLoader();
        try {
            thread.setContextClassLoader(loader);
            Class<? extends Plugin> type = Class.forName(entry, false, loader).asSubclass(Plugin.class);
            return type.getConstructor().newInstance();
        } finally {
            thread.setContextClassLoader(previous);
        }
    }

    private JsonArray dispatch(String target, Event event) {
        JsonArray results = new JsonArray();
        if (target != null) {
            if (paper != null && paper.contains(target)) return paper.dispatch(target, event);
            LoadedPlugin plugin = plugins.get(target);
            if (plugin == null) throw new IllegalArgumentException("plugin not found: " + target);
            if (plugin.enabled()) results.add(invoke(plugin, event));
            else results.add(result(target, false, new JsonArray(), "plugin is disabled: " + plugin.error));
        } else {
            for (LoadedPlugin plugin : plugins.values()) if (plugin.enabled()) results.add(invoke(plugin, event));
            if (paper != null) results.addAll(paper.dispatch(null, event));
        }
        return results;
    }

    private static JsonObject invoke(LoadedPlugin plugin, Event event) {
        Thread thread = Thread.currentThread();
        ClassLoader previous = thread.getContextClassLoader();
        try {
            thread.setContextClassLoader(plugin.loader);
            if (event instanceof Event.Command command && plugin.permissions.containsKey(command.command()) && !command.operator() && command.senderKind() != Event.SenderKind.CONSOLE) {
                return result(plugin.name, true, Wire.actions(List.of(new io.github.teighto.zerqon.api.Action.Reply("Permission denied")), event), null);
            }
            JsonArray actions = Wire.actions(plugin.instance.onEvent(event), event);
            if (event instanceof Event.Enable) {
                for (JsonElement action : actions) {
                    JsonObject value = action.getAsJsonObject();
                    if (value.get("action").getAsString().equals("register_command") && !value.get("permission").isJsonNull()) plugin.permissions.put(value.get("name").getAsString(), value.get("permission").getAsString());
                }
            }
            return result(plugin.name, true, actions, null);
        } catch (Exception | LinkageError | AssertionError error) {
            if (error instanceof InterruptedException) thread.interrupt();
            plugin.error = Wire.error(error);
            plugin.instance = null;
            closeLoader(plugin.loader, error);
            return result(plugin.name, false, new JsonArray(), plugin.error);
        } finally {
            thread.setContextClassLoader(previous);
        }
    }

    private JsonArray shutdown() {
        JsonArray results = new JsonArray();
        for (LoadedPlugin plugin : plugins.values()) {
            JsonObject result = plugin.enabled() ? invoke(plugin, new Event.Disable()) : result(plugin.name, false, new JsonArray(), plugin.error);
            plugin.instance = null;
            try {
                plugin.loader.close();
            } catch (IOException error) {
                result.addProperty("error", Wire.error(error));
                System.err.println("Java plugin " + plugin.name + ": " + Wire.error(error));
            }
            result.addProperty("enabled", false);
            results.add(result);
        }
        plugins.clear();
        if (paper != null) results.addAll(paper.shutdown());
        return results;
    }

    @Override
    public void close() {
        shutdown();
    }

    private static void closeLoader(URLClassLoader loader, Throwable original) {
        try {
            loader.close();
        } catch (IOException failure) {
            original.addSuppressed(failure);
            System.err.println("Java plugin classloader close failed: " + Wire.error(failure));
        }
    }

    private static JsonObject response(JsonElement id, boolean ok, JsonArray results) {
        JsonObject response = new JsonObject();
        response.add("id", id);
        response.addProperty("ok", ok);
        response.add("results", results);
        return response;
    }

    private static JsonObject result(String name, boolean enabled, JsonArray actions, String error) {
        JsonObject result = new JsonObject();
        result.addProperty("name", name);
        result.addProperty("enabled", enabled);
        result.add("actions", actions);
        if (error != null) result.addProperty("error", error);
        return result;
    }

    private static byte[] readLine(InputStream input) throws IOException {
        ByteArrayOutputStream line = new ByteArrayOutputStream(256);
        int next;
        while ((next = input.read()) != -1) {
            if (next == '\n') return line.toByteArray();
            if (line.size() == MAX_LINE_BYTES) throw new IOException("request exceeds 64 KiB");
            line.write(next);
        }
        if (line.size() != 0) throw new IOException("unterminated protocol request");
        return null;
    }

    private static final class LoadedPlugin {
        private final String name;
        private final URLClassLoader loader;
        private Plugin instance;
        private String error;
        private final Map<String, String> permissions = new TreeMap<>();

        private LoadedPlugin(String name, URLClassLoader loader, Plugin instance) {
            this.name = name;
            this.loader = loader;
            this.instance = instance;
        }

        private boolean enabled() { return instance != null; }
    }
}
