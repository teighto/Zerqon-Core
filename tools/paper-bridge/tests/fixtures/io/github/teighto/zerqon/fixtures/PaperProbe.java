package io.github.teighto.zerqon.fixtures;

import java.util.Objects;
import org.bukkit.Bukkit;
import org.bukkit.command.Command;
import org.bukkit.command.CommandSender;
import org.bukkit.event.Cancellable;
import org.bukkit.event.Event;
import org.bukkit.event.EventHandler;
import org.bukkit.event.EventPriority;
import org.bukkit.event.HandlerList;
import org.bukkit.event.Listener;
import org.bukkit.plugin.java.JavaPlugin;

public final class PaperProbe extends JavaPlugin implements Listener {
    private int ticks;
    private int calls;
    private String order = "";

    public PaperProbe() { checkContext(); }
    private void checkContext() { if (Thread.currentThread().getContextClassLoader() != getClass().getClassLoader()) { throw new IllegalStateException("wrong Paper plugin context classloader"); } }
    @Override public void onLoad() { checkContext(); System.setProperty("zerqon.probe.loaded", "true"); }
    @Override public void onEnable() {
        checkContext();
        saveDefaultConfig();
        getConfig().set("enabled", true);
        saveConfig();
        Objects.requireNonNull(getCommand("probe")).setExecutor(this);
        Bukkit.getPluginManager().registerEvents(this, this);
        Bukkit.getScheduler().runTaskTimer(this, () -> { checkContext(); ticks++; }, 1, 2);
        Bukkit.getScheduler().runTaskLater(this, () -> { calls++; }, 2);
    }
    @Override public void onDisable() { checkContext(); System.setProperty("zerqon.probe.disabled", "true"); }

    @Override public boolean onCommand(CommandSender sender, Command command, String label, String[] args) {
        checkContext();
        if (args.length > 0 && args[0].equals("event")) {
            ProbeEvent event = new ProbeEvent();
            Bukkit.getPluginManager().callEvent(event);
            sender.sendMessage(order + ":" + event.isCancelled());
        } else {
            sender.sendMessage(getConfig().getString("greeting") + ":" + ticks + ":" + calls + ":" + sender.getName() + ":" + label);
        }
        return true;
    }

    @EventHandler(priority = EventPriority.HIGH, ignoreCancelled = true)
    public void ignored(ProbeEvent event) { order += "unexpected"; }
    @EventHandler(priority = EventPriority.LOW)
    public void cancel(ProbeEvent event) { order += "low"; event.setCancelled(true); }
    @EventHandler(priority = EventPriority.MONITOR)
    public void monitor(ProbeEvent event) { checkContext(); order += "monitor"; }

    public static final class ProbeEvent extends Event implements Cancellable {
        private static final HandlerList HANDLERS = new HandlerList();
        private boolean cancelled;
        @Override public HandlerList getHandlers() { return HANDLERS; }
        public static HandlerList getHandlerList() { return HANDLERS; }
        @Override public boolean isCancelled() { return cancelled; }
        @Override public void setCancelled(boolean value) { cancelled = value; }
    }
}
