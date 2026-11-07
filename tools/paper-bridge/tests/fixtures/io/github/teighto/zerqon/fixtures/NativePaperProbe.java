package io.github.teighto.zerqon.fixtures;

import org.bukkit.Bukkit;
import org.bukkit.command.Command;
import org.bukkit.command.CommandSender;
import org.bukkit.event.Event;
import org.bukkit.event.EventHandler;
import org.bukkit.event.HandlerList;
import org.bukkit.event.Listener;
import org.bukkit.plugin.java.JavaPlugin;

public final class NativePaperProbe extends JavaPlugin implements Listener {
    private static long snapshot() {
        try {
            Object snapshot = Class.forName("io.github.teighto.zerqon.api.NativeQueries").getMethod("serverSnapshot").invoke(null);
            return (Long) snapshot.getClass().getMethod("tick").invoke(snapshot);
        } catch (ReflectiveOperationException failure) {
            throw new IllegalStateException("Native Paper query failed", failure);
        }
    }

    @Override public void onLoad() { snapshot(); }
    @Override public void onEnable() {
        snapshot();
        Bukkit.getPluginManager().registerEvents(this, this);
        Bukkit.getScheduler().runTask(this, NativePaperProbe::snapshot);
    }
    @Override public void onDisable() { snapshot(); }
    @Override public boolean onCommand(CommandSender sender, Command command, String label, String[] args) {
        if (args.length == 2 && args[0].equals("nested")) {
            Bukkit.dispatchCommand(sender, args[1]);
        } else {
            Bukkit.getPluginManager().callEvent(new ProbeEvent());
        }
        sender.sendMessage("tick=" + snapshot());
        return true;
    }
    @EventHandler public void probe(ProbeEvent event) { snapshot(); }

    public static final class ProbeEvent extends Event {
        private static final HandlerList HANDLERS = new HandlerList();
        @Override public HandlerList getHandlers() { return HANDLERS; }
        public static HandlerList getHandlerList() { return HANDLERS; }
    }
}
