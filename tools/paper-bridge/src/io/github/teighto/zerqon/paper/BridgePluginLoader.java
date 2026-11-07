package io.github.teighto.zerqon.paper;

import java.io.File;
import java.io.IOException;
import java.util.Map;
import java.util.Set;
import java.util.regex.Pattern;
import org.bukkit.event.Event;
import org.bukkit.event.Listener;
import org.bukkit.plugin.InvalidDescriptionException;
import org.bukkit.plugin.InvalidPluginException;
import org.bukkit.plugin.Plugin;
import org.bukkit.plugin.PluginDescriptionFile;
import org.bukkit.plugin.PluginLoader;
import org.bukkit.plugin.RegisteredListener;
import org.bukkit.plugin.java.JavaPlugin;

@SuppressWarnings({"deprecation", "removal"})
public final class BridgePluginLoader implements PluginLoader {
    public BridgePluginLoader() {}
    @Override public Plugin loadPlugin(File file) throws InvalidPluginException {
        try { return PaperBridge.current().load(file.toPath()); }
        catch (Exception failure) { throw new InvalidPluginException(failure); }
    }
    @Override public PluginDescriptionFile getPluginDescription(File file) throws InvalidDescriptionException {
        try { return PaperBridge.description(file.toPath()); }
        catch (IOException failure) { throw new InvalidDescriptionException(failure); }
    }
    @Override public Pattern[] getPluginFileFilters() { return new Pattern[]{Pattern.compile("\\.jar$")}; }
    @Override public Map<Class<? extends Event>, Set<RegisteredListener>> createRegisteredListeners(Listener listener, Plugin plugin) { return PaperBridge.current().listeners(listener, plugin); }
    @Override public void enablePlugin(Plugin plugin) { PaperBridge.current().enable((JavaPlugin) plugin); }
    @Override public void disablePlugin(Plugin plugin) { PaperBridge.current().disable((JavaPlugin) plugin); }
}
