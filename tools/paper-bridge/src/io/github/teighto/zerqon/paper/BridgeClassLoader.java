package io.github.teighto.zerqon.paper;

import io.papermc.paper.plugin.configuration.PluginMeta;
import io.papermc.paper.plugin.provider.classloader.ConfiguredPluginClassLoader;
import io.papermc.paper.plugin.provider.classloader.PluginClassLoaderGroup;
import java.io.IOException;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Path;
import java.util.List;
import java.util.logging.Logger;
import org.bukkit.Server;
import org.bukkit.plugin.PluginDescriptionFile;
import org.bukkit.plugin.java.JavaPlugin;

final class BridgeClassLoader extends URLClassLoader implements ConfiguredPluginClassLoader {
    private final Path file;
    private final Path data;
    private final PluginDescriptionFile meta;
    private final Server server;
    private final PaperBridge bridge;
    private final List<BridgeClassLoader> dependencies;
    private JavaPlugin plugin;

    BridgeClassLoader(Path file, Path data, PluginDescriptionFile meta, Server server, PaperBridge bridge, List<BridgeClassLoader> dependencies) throws IOException {
        super(new URL[]{file.toUri().toURL()}, BridgeClassLoader.class.getClassLoader());
        this.file = file; this.data = data; this.meta = meta; this.server = server; this.bridge = bridge;
        this.dependencies = List.copyOf(dependencies);
    }
    JavaPlugin create() throws ReflectiveOperationException { return loadClass(meta.getMain()).asSubclass(JavaPlugin.class).getDeclaredConstructor().newInstance(); }
    @Override public PluginMeta getConfiguration() { return meta; }
    @Override protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
        try { return super.loadClass(name, resolve); }
        catch (ClassNotFoundException missing) {
            for (BridgeClassLoader dependency : dependencies) {
                try { return dependency.loadClass(name, resolve); }
                catch (ClassNotFoundException ignored) { missing.addSuppressed(ignored); }
            }
            throw missing;
        }
    }
    @Override public Class<?> loadClass(String name, boolean resolve, boolean checkGlobal, boolean checkLibraries) throws ClassNotFoundException {
        return checkGlobal ? loadClass(name, resolve) : super.loadClass(name, resolve);
    }
    @Override public void init(JavaPlugin value) {
        bridge.checkThread();
        if (plugin != null) { throw new IllegalStateException("Plugin already initialized"); }
        plugin = value;
        value.init(server, meta, data.toFile(), file.toFile(), this, meta, Logger.getLogger(meta.getName()));
    }
    @Override public JavaPlugin getPlugin() { return plugin; }
    @Override public PluginClassLoaderGroup getGroup() { return null; }
}
