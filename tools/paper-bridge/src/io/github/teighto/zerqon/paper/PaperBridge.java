package io.github.teighto.zerqon.paper;

import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;
import java.util.jar.JarFile;
import java.util.logging.Level;
import java.util.logging.Logger;
import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.serializer.plain.PlainTextComponentSerializer;
import org.bukkit.Bukkit;
import org.bukkit.Server;
import org.bukkit.command.Command;
import org.bukkit.command.CommandSender;
import org.bukkit.command.ConsoleCommandSender;
import org.bukkit.command.PluginCommand;
import org.bukkit.command.PluginCommandYamlParser;
import org.bukkit.event.Event;
import org.bukkit.event.EventHandler;
import org.bukkit.event.EventPriority;
import org.bukkit.event.HandlerList;
import org.bukkit.event.Listener;
import org.bukkit.event.server.PluginDisableEvent;
import org.bukkit.event.server.PluginEnableEvent;
import org.bukkit.permissions.Permission;
import org.bukkit.permissions.PermissionDefault;
import org.bukkit.plugin.EventExecutor;
import org.bukkit.plugin.InvalidDescriptionException;
import org.bukkit.plugin.Plugin;
import org.bukkit.plugin.PluginDescriptionFile;
import org.bukkit.plugin.PluginManager;
import org.bukkit.plugin.RegisteredListener;
import org.bukkit.plugin.java.JavaPlugin;

@SuppressWarnings("deprecation")
public final class PaperBridge implements AutoCloseable {
    private static PaperBridge instance;
    private final Thread owner = Thread.currentThread();
    private final Path directory;
    private final CallbackContext callbackContext;
    private final ReadOnlyServerView nativeView;
    private final BridgeReadModel readModel;
    private PlayerReplies playerReplies;
    private final Logger logger = Logger.getLogger("Zerqon-Paper");
    private final Map<String, BridgeClassLoader> plugins = new LinkedHashMap<>();
    private final Map<String, PluginCommand> commands = new LinkedHashMap<>();
    private final Map<String, Permission> permissions = new LinkedHashMap<>();
    private final Map<Plugin, Set<String>> ownedPermissions = new LinkedHashMap<>();
    private final BridgeScheduler scheduler = new BridgeScheduler(this);
    private final PluginManager manager = proxy(PluginManager.class, this::managerCall);
    private final Server server = proxy(Server.class, this::serverCall);
    private boolean closed;

    public PaperBridge(Path directory) {
        this(directory, plugin -> () -> {});
    }

    public PaperBridge(Path directory, CallbackContext callbackContext) {
        this(directory, callbackContext, null);
    }

    public PaperBridge(Path directory, CallbackContext callbackContext, ReadOnlyServerView nativeView) {
        if (instance != null || Bukkit.getServer() != null) { throw new IllegalStateException("Only one Paper bridge may be installed in a JVM"); }
        this.directory = directory.toAbsolutePath().normalize();
        this.callbackContext = java.util.Objects.requireNonNull(callbackContext, "callbackContext");
        this.nativeView = nativeView;
        this.readModel = nativeView == null ? null : new BridgeReadModel(this, nativeView);
        instance = this;
        Bukkit.setServer(server);
    }

    @FunctionalInterface
    public interface CallbackContext { CallbackScope enter(Plugin plugin); }
    @FunctionalInterface
    public interface CallbackScope extends AutoCloseable { @Override void close(); }

    Server server() { return server; }

    public void playerJoined(ReadOnlyServerView.PlayerData player) {
        checkThread();
        if (readModel == null) throw new UnsupportedOperationException("Native Paper PlayerJoinEvent adapter requires a native view");
        fire(new org.bukkit.event.player.PlayerJoinEvent(readModel.player(player), (Component) null));
    }

    public void playerQuit(ReadOnlyServerView.PlayerData player) {
        checkThread();
        if (readModel == null) throw new UnsupportedOperationException("Native Paper PlayerQuitEvent adapter requires a native view");
        try { fire(new org.bukkit.event.player.PlayerQuitEvent(readModel.player(player), (Component) null, org.bukkit.event.player.PlayerQuitEvent.QuitReason.DISCONNECTED)); }
        finally { readModel.quit(player); }
    }

    static PaperBridge current() {
        if (instance == null) { throw new IllegalStateException("Paper bridge is not installed"); }
        return instance;
    }

    void checkThread() {
        if (Thread.currentThread() != owner) { throw new IllegalStateException("Paper API called outside its synchronous host thread"); }
        if (closed) { throw new IllegalStateException("Paper bridge is closed"); }
    }

    static UnsupportedOperationException unsupported(Method method) {
        return new UnsupportedOperationException("Zerqon Paper API is not implemented: " + method.toGenericString());
    }

    static <T> T context(Plugin plugin, java.util.function.Supplier<T> action) {
        Thread thread = Thread.currentThread();
        ClassLoader previous = thread.getContextClassLoader();
        CallbackScope scope = current().callbackContext.enter(plugin);
        try { thread.setContextClassLoader(plugin.getClass().getClassLoader()); return action.get(); }
        finally { scope.close(); thread.setContextClassLoader(previous); }
    }

    static <T> T proxy(Class<T> type, InvocationHandler implementation) {
        return type.cast(Proxy.newProxyInstance(type.getClassLoader(), new Class<?>[]{type}, (proxy, method, args) -> {
            if (method.getDeclaringClass() == Object.class) {
                return switch (method.getName()) {
                    case "equals" -> proxy == args[0];
                    case "hashCode" -> System.identityHashCode(proxy);
                    case "toString" -> "Zerqon " + type.getSimpleName();
                    default -> throw unsupported(method);
                };
            }
            return implementation.invoke(proxy, method, args == null ? new Object[0] : args);
        }));
    }

    public static PluginDescriptionFile description(Path file) throws IOException, InvalidDescriptionException {
        try (JarFile jar = new JarFile(file.toFile())) {
            var entry = jar.getJarEntry("plugin.yml");
            if (entry == null) { throw new InvalidDescriptionException("plugin.yml is required; paper-plugin.yml bootstrap is not implemented"); }
            try (InputStream stream = jar.getInputStream(entry)) {
                byte[] bytes = stream.readNBytes(1_048_577);
                if (bytes.length > 1_048_576) { throw new InvalidDescriptionException("plugin.yml exceeds 1 MiB"); }
                return new PluginDescriptionFile(new java.io.ByteArrayInputStream(bytes));
            }
        }
    }

    public JavaPlugin load(Path file) throws Exception {
        checkThread();
        if (plugins.size() >= 64) { throw new IllegalStateException("Paper plugin limit is 64"); }
        PluginDescriptionFile meta = description(file);
        if (!meta.getName().matches("[A-Za-z0-9_-]{1,64}")) { throw new InvalidDescriptionException("Invalid plugin name"); }
        if (!"26.3".equals(meta.getAPIVersion())) { throw new InvalidDescriptionException("This experimental bridge requires api-version: '26.3'"); }
        if (!meta.getLibraries().isEmpty()) { throw new InvalidDescriptionException("plugin.yml libraries are not implemented; shade required libraries into the plugin"); }
        String key = meta.getName().toLowerCase(Locale.ROOT);
        if (plugins.containsKey(key)) { throw new IllegalArgumentException("Duplicate plugin " + meta.getName()); }
        List<BridgeClassLoader> dependencies = new ArrayList<>();
        for (String dependency : meta.getDepend()) {
            if (!plugins.containsKey(dependency.toLowerCase(Locale.ROOT))) { throw new IllegalStateException("Missing plugin dependency " + dependency); }
            dependencies.add(plugins.get(dependency.toLowerCase(Locale.ROOT)));
        }
        for (String dependency : meta.getSoftDepend()) {
            BridgeClassLoader available = plugins.get(dependency.toLowerCase(Locale.ROOT));
            if (available != null) { dependencies.add(available); }
        }
        Path data = directory.resolve("config").resolve(meta.getName());
        Files.createDirectories(data);
        BridgeClassLoader loader = new BridgeClassLoader(file.toAbsolutePath(), data, meta, server, this, dependencies);
        plugins.put(key, loader);
        Thread thread = Thread.currentThread();
        ClassLoader previous = thread.getContextClassLoader();
        try {
            thread.setContextClassLoader(loader);
            JavaPlugin plugin = loader.create();
            context(plugin, () -> { plugin.onLoad(); return null; });
            return plugin;
        } catch (Exception | Error failure) {
            plugins.remove(key);
            try { loader.close(); } catch (IOException cleanup) { failure.addSuppressed(cleanup); }
            throw failure;
        } finally {
            thread.setContextClassLoader(previous);
        }
    }

    public void enable(JavaPlugin plugin) {
        checkThread(); requireOwned(plugin);
        if (plugin.isEnabled()) { return; }
        try {
            Set<String> added = new LinkedHashSet<>();
            ownedPermissions.put(plugin, added);
            for (Permission permission : plugin.getDescription().getPermissions()) {
                if (!permission.getChildren().isEmpty()) { throw new UnsupportedOperationException("Zerqon Paper API is not implemented: permission children for " + permission.getName()); }
                if (permissions.putIfAbsent(permission.getName(), permission) != null) { throw new IllegalArgumentException("Permission already registered: " + permission.getName()); }
                added.add(permission.getName());
            }
            for (Command parsed : PluginCommandYamlParser.parse(plugin)) {
                PluginCommand command = (PluginCommand) parsed;
                String namespace = plugin.getName().toLowerCase(Locale.ROOT) + ":";
                commands.put(namespace + command.getName(), command);
                commands.putIfAbsent(command.getName(), command);
                for (String alias : command.getAliases()) {
                    commands.putIfAbsent(alias.toLowerCase(Locale.ROOT), command);
                    commands.put(namespace + alias.toLowerCase(Locale.ROOT), command);
                }
            }
            context(plugin, () -> { plugin.setEnabled(true); return null; });
            fire(new PluginEnableEvent(plugin));
        } catch (RuntimeException | Error failure) {
            try { disable(plugin); } catch (RuntimeException | Error cleanup) { failure.addSuppressed(cleanup); }
            throw failure;
        }
    }

    public void disable(JavaPlugin plugin) {
        checkThread();
        BridgeClassLoader loader = requireOwned(plugin);
        try {
            if (plugin.isEnabled()) { fire(new PluginDisableEvent(plugin)); context(plugin, () -> { plugin.setEnabled(false); return null; }); }
        } finally {
            scheduler.cancel(plugin);
            HandlerList.unregisterAll(plugin);
            commands.values().removeIf(command -> command.getPlugin() == plugin);
            Set<String> removed = ownedPermissions.remove(plugin);
            if (removed != null) { removed.forEach(permissions::remove); }
            plugins.remove(plugin.getName().toLowerCase(Locale.ROOT));
            try { loader.close(); } catch (IOException failure) { throw new IllegalStateException("Cannot close plugin classloader", failure); }
        }
    }

    private BridgeClassLoader requireOwned(JavaPlugin plugin) {
        BridgeClassLoader loader = plugins.get(plugin.getName().toLowerCase(Locale.ROOT));
        if (loader == null || loader.getPlugin() != plugin) { throw new IllegalArgumentException("Plugin does not belong to this bridge"); }
        return loader;
    }

    public void tick(long tick) { checkThread(); scheduler.tick(tick); }

    public List<String> execute(String name, boolean operator, boolean console, String command) {
        checkThread();
        List<String> replies = new ArrayList<>();
        CommandSender sender = sender(name, operator, console, replies);
        dispatch(sender, command);
        return List.copyOf(replies);
    }

    public record CommandInfo(String name, String description, String permission) {}

    public List<CommandInfo> commands(JavaPlugin plugin) {
        checkThread(); requireOwned(plugin);
        return commands.entrySet().stream().filter(entry -> entry.getValue().getPlugin() == plugin && !entry.getKey().contains(":"))
            .map(entry -> new CommandInfo(entry.getKey(), entry.getValue().getDescription(), entry.getValue().getPermission())).toList();
    }

    public List<String> executeFor(JavaPlugin plugin, String name, boolean operator, boolean console, String label, List<String> args) {
        checkThread(); requireOwned(plugin);
        PluginCommand command = commands.get(label.toLowerCase(Locale.ROOT));
        if (command == null || command.getPlugin() != plugin) { throw new IllegalArgumentException("Command does not belong to plugin: " + label); }
        List<String> replies = new ArrayList<>();
        PlayerReplies previous = playerReplies;
        ReadOnlyServerView.PlayerData player = nativeView == null ? null : nativeView.eventPlayer();
        playerReplies = !console && player != null ? new PlayerReplies(player.uuid(), player.sessionId(), replies) : null;
        try { context(plugin, () -> command.execute(sender(name, operator, console, replies), label, args.toArray(String[]::new))); }
        finally { playerReplies = previous; }
        return List.copyOf(replies);
    }

    public boolean hasListeners(JavaPlugin plugin, Class<? extends Event> event) {
        checkThread();
        return Arrays.stream(handlers(event).getRegisteredListeners()).anyMatch(listener -> listener.getPlugin() == plugin);
    }

    private boolean dispatch(CommandSender sender, String input) {
        String[] parts = input.strip().replaceFirst("^/", "").split("\\s+");
        PluginCommand command = commands.get(parts[0].toLowerCase(Locale.ROOT));
        if (command == null) { sender.sendMessage("Unknown command"); return false; }
        return context(command.getPlugin(), () -> command.execute(sender, parts[0], Arrays.copyOfRange(parts, 1, parts.length)));
    }

    private CommandSender sender(String name, boolean operator, boolean console, List<String> replies) {
        ReadOnlyServerView.PlayerData player = nativeView == null ? null : nativeView.eventPlayer();
        if (!console && player != null && readModel != null) return readModel.player(player);
        InvocationHandler implementation = (object, method, args) -> {
            checkThread();
            return switch (method.getName()) {
                case "getName" -> name;
                case "getServer" -> server;
                case "isOp" -> operator || console;
                case "hasPermission" -> hasPermission(args[0] instanceof Permission permission ? permission.getName() : (String) args[0], operator || console);
                case "isPermissionSet" -> permissions.containsKey(args[0] instanceof Permission permission ? permission.getName() : args[0]);
                case "sendMessage", "sendPlainMessage" -> {
                    Object message = args[args.length - 1];
                    if (message instanceof String text) { addReply(replies, text); }
                    else if (message instanceof String[] messages) { for (String text : messages) { addReply(replies, text); } }
                    else if (message instanceof Component component) { addReply(replies, PlainTextComponentSerializer.plainText().serialize(component)); }
                    else { throw unsupported(method); }
                    yield null;
                }
                default -> throw unsupported(method);
            };
        };
        return console ? proxy(ConsoleCommandSender.class, implementation) : proxy(CommandSender.class, implementation);
    }

    private static void addReply(List<String> replies, String text) {
        if (replies.size() >= 32 || text.getBytes(java.nio.charset.StandardCharsets.UTF_8).length > 4096) { throw new IllegalArgumentException("Paper command reply limit exceeded"); }
        replies.add(text);
    }

    boolean hasPermission(String name, boolean operator) {
        Permission permission = permissions.get(name.toLowerCase(Locale.ROOT));
        return (permission == null ? PermissionDefault.OP : permission.getDefault()).getValue(operator);
    }

    boolean permissionSet(String name) { return permissions.containsKey(name.toLowerCase(Locale.ROOT)); }

    private record PlayerReplies(java.util.UUID uuid, int session, List<String> replies) {}

    void playerReply(java.util.UUID uuid, int session, Method method, Object[] args) {
        if (playerReplies == null || !playerReplies.uuid().equals(uuid) || playerReplies.session() != session) throw unsupported(method);
        Object message = args[args.length - 1];
        if (message instanceof String text) addReply(playerReplies.replies(), text);
        else if (message instanceof String[] messages) { for (String text : messages) addReply(playerReplies.replies(), text); }
        else if (message instanceof Component component) addReply(playerReplies.replies(), PlainTextComponentSerializer.plainText().serialize(component));
        else throw unsupported(method);
    }

    public void fire(Event event) {
        checkThread();
        if (event.isAsynchronous()) { throw new UnsupportedOperationException("Asynchronous Bukkit events are not implemented"); }
        for (RegisteredListener listener : event.getHandlers().getRegisteredListeners()) {
            if (!listener.getPlugin().isEnabled()) { continue; }
            Thread thread = Thread.currentThread();
            ClassLoader previous = thread.getContextClassLoader();
            CallbackScope scope = callbackContext.enter(listener.getPlugin());
            try { thread.setContextClassLoader(listener.getPlugin().getClass().getClassLoader()); listener.callEvent(event); }
            catch (org.bukkit.event.EventException failure) { logger.log(Level.SEVERE, "Plugin event handler failed: " + listener.getPlugin().getName(), failure.getCause()); }
            finally { scope.close(); thread.setContextClassLoader(previous); }
        }
    }

    Map<Class<? extends Event>, Set<RegisteredListener>> listeners(Listener listener, Plugin plugin) {
        checkThread();
        Map<Class<? extends Event>, Set<RegisteredListener>> result = new LinkedHashMap<>();
        Set<Method> methods = new LinkedHashSet<>(List.of(listener.getClass().getMethods()));
        methods.addAll(List.of(listener.getClass().getDeclaredMethods()));
        for (Method method : methods) {
            EventHandler handler = method.getAnnotation(EventHandler.class);
            if (handler == null || method.isBridge() || method.isSynthetic()) { continue; }
            if (method.getParameterCount() != 1 || !Event.class.isAssignableFrom(method.getParameterTypes()[0])) { throw new IllegalArgumentException("Invalid event handler: " + method); }
            Class<? extends Event> eventType = method.getParameterTypes()[0].asSubclass(Event.class);
            if (!method.trySetAccessible()) { throw new IllegalArgumentException("Event handler is inaccessible: " + method); }
            EventExecutor executor = (target, event) -> {
                if (!eventType.isInstance(event)) { return; }
                try { method.invoke(target, event); }
                catch (InvocationTargetException failure) { throw new org.bukkit.event.EventException(failure.getCause()); }
                catch (ReflectiveOperationException failure) { throw new org.bukkit.event.EventException(failure); }
            };
            result.computeIfAbsent(eventType, ignored -> new LinkedHashSet<>()).add(new RegisteredListener(listener, executor, handler.priority(), plugin, handler.ignoreCancelled()));
        }
        return result;
    }

    private static HandlerList handlers(Class<?> event) {
        try { return (HandlerList) event.getMethod("getHandlerList").invoke(null); }
        catch (ReflectiveOperationException failure) { throw new IllegalArgumentException("Event requires a static getHandlerList: " + event.getName(), failure); }
    }

    private Object managerCall(Object object, Method method, Object[] args) throws Exception {
        checkThread();
        return switch (method.getName()) {
            case "getPlugin" -> { BridgeClassLoader loader = plugins.get(((String) args[0]).toLowerCase(Locale.ROOT)); yield loader == null ? null : loader.getPlugin(); }
            case "getPlugins" -> plugins.values().stream().map(BridgeClassLoader::getPlugin).filter(java.util.Objects::nonNull).toArray(Plugin[]::new);
            case "isPluginEnabled" -> { Plugin plugin = args[0] instanceof Plugin value ? value : manager.getPlugin((String) args[0]); yield plugin != null && plugin.isEnabled(); }
            case "enablePlugin" -> { enable((JavaPlugin) args[0]); yield null; }
            case "disablePlugin" -> { disable((JavaPlugin) args[0]); yield null; }
            case "callEvent" -> { fire((Event) args[0]); yield null; }
            case "registerEvents" -> {
                Plugin plugin = (Plugin) args[1];
                if (!plugin.isEnabled()) { throw new IllegalStateException("Cannot register listeners for a disabled plugin"); }
                listeners((Listener) args[0], plugin).forEach((type, registered) -> handlers(type).registerAll(registered)); yield null;
            }
            case "registerEvent" -> {
                Plugin plugin = (Plugin) args[4];
                if (!plugin.isEnabled()) { throw new IllegalStateException("Cannot register listeners for a disabled plugin"); }
                handlers((Class<?>) args[0]).register(new RegisteredListener((Listener) args[1], (EventExecutor) args[3], (EventPriority) args[2], plugin, args.length == 6 && (Boolean) args[5])); yield null;
            }
            case "getPermission" -> permissions.get((String) args[0]);
            case "getPermissions" -> Set.copyOf(permissions.values());
            default -> throw unsupported(method);
        };
    }

    private Object serverCall(Object object, Method method, Object[] args) {
        if (method.getName().equals("isPrimaryThread")) { return Thread.currentThread() == owner; }
        checkThread();
        return switch (method.getName()) {
            case "getName" -> "Zerqon";
            case "getVersion" -> "Zerqon 0.0.1 (MC: 26.3)";
            case "getBukkitVersion" -> "26.3.build.242-stable";
            case "getMinecraftVersion" -> "26.3";
            case "getLogger" -> logger;
            case "getPluginManager" -> manager;
            case "getScheduler" -> scheduler.api();
            case "getPluginCommand" -> commands.get(((String) args[0]).toLowerCase(Locale.ROOT));
            case "getOnlinePlayers" -> { if (readModel == null) throw unsupported(method); yield readModel.onlinePlayers(); }
            case "getPlayer" -> { if (readModel == null) throw unsupported(method); yield args[0] instanceof java.util.UUID uuid ? readModel.player(uuid) : readModel.player((String) args[0], false); }
            case "getPlayerExact" -> { if (readModel == null) throw unsupported(method); yield readModel.player((String) args[0], true); }
            case "getWorlds" -> { if (readModel == null) throw unsupported(method); yield readModel.worlds(); }
            case "getWorld" -> { if (readModel == null) throw unsupported(method); yield readModel.world(args[0]); }
            case "getPluginsFolder" -> directory.resolve("plugins/java-plugins").toFile();
            case "getCurrentTick" -> Math.toIntExact(scheduler.currentTick());
            case "permissionMessage" -> Component.text("Permission denied");
            case "dispatchCommand" -> dispatch((CommandSender) args[0], (String) args[1]);
            default -> throw unsupported(method);
        };
    }

    @Override public void close() {
        checkThread();
        RuntimeException failure = null;
        for (BridgeClassLoader loader : new ArrayList<>(plugins.values()).reversed()) {
            try { disable(loader.getPlugin()); }
            catch (RuntimeException exception) { if (failure == null) { failure = exception; } else { failure.addSuppressed(exception); } }
        }
        closed = true;
        if (failure != null) { throw failure; }
    }
}
