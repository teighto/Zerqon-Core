package io.github.teighto.zerqon.fixtures;

import org.bukkit.Bukkit;
import org.bukkit.Location;
import org.bukkit.World;
import org.bukkit.command.Command;
import org.bukkit.command.CommandSender;
import org.bukkit.entity.Player;
import org.bukkit.event.EventHandler;
import org.bukkit.event.Listener;
import org.bukkit.event.player.PlayerJoinEvent;
import org.bukkit.event.player.PlayerQuitEvent;
import org.bukkit.plugin.java.JavaPlugin;

public final class ReadOnlyPaperProbe extends JavaPlugin implements Listener {
    private int joins;
    private int quits;
    private Player previous;
    private final java.util.Map<java.util.UUID, Player> joined = new java.util.HashMap<>();
    private java.util.Collection<? extends Player> onlineView;

    @Override public void onEnable() { Bukkit.getPluginManager().registerEvents(this, this); }
    @EventHandler public void join(PlayerJoinEvent event) {
        joins++;
        if (!event.getPlayer().isOnline()) throw new AssertionError("join player offline");
        joined.put(event.getPlayer().getUniqueId(), event.getPlayer());
        event.getPlayer().getLocation();
    }
    @EventHandler public void quit(PlayerQuitEvent event) {
        quits++;
        if (joined.remove(event.getPlayer().getUniqueId()) != event.getPlayer() || !event.getPlayer().isOnline()) throw new AssertionError("quit player identity/status");
        previous = event.getPlayer();
        event.getPlayer().getLocation();
    }
    @Override public boolean onCommand(CommandSender sender, Command command, String label, String[] args) {
        if (args.length > 0 && args[0].equals("counts")) {
            sender.sendMessage(joins + ":" + quits); return true;
        }
        if (args.length > 0 && (args[0].equals("save-view") || args[0].equals("view"))) {
            if (args[0].equals("save-view")) onlineView = Bukkit.getOnlinePlayers();
            sender.sendMessage(Integer.toString(java.util.Objects.requireNonNull(onlineView).size())); return true;
        }
        if (args.length > 0 && args[0].equals("scan")) {
            int count = 0;
            for (Player online : Bukkit.getOnlinePlayers()) {
                if (!online.isOnline() || !online.isValid()) throw new AssertionError("online snapshot became invalid");
                online.getLocation(); online.getWorld(); online.isOp(); online.hasPermission("native.public");
                count++;
            }
            sender.sendMessage("scanned " + count); return true;
        }
        if (args.length > 1 && args[0].equals("seed-prior")) {
            Player player = java.util.Objects.requireNonNull(Bukkit.getPlayer(java.util.UUID.fromString(args[1])));
            double before = player.getLocation().getX();
            Bukkit.getOnlinePlayers().size();
            Player after = java.util.Objects.requireNonNull(Bukkit.getPlayer(player.getUniqueId()));
            if (after != player || after.getLocation().getX() != before) throw new AssertionError("list changed an earlier parent snapshot");
            sender.sendMessage("preserved " + before); return true;
        }
        if (args.length > 0 && args[0].equals("world")) {
            World world = sender instanceof Player player ? player.getWorld() : Bukkit.getWorlds().getFirst();
            Location spawn = world.getSpawnLocation();
            sender.sendMessage(world.getUID() + ":" + world.getKey() + ":" + world.getEnvironment() + ":" + world.getMinHeight() + ":" + world.getMaxHeight()
                    + ":" + spawn.getX() + ":" + spawn.getY() + ":" + spawn.getZ() + ":" + spawn.getYaw() + ":" + spawn.getPitch());
            return true;
        }
        if (!(sender instanceof Player player)) { sender.sendMessage("not-player"); return true; }
        if (args.length > 0 && args[0].equals("null-location")) {
            if (player.getLocation(null) != null) throw new AssertionError("null location contract");
            sender.sendMessage("null"); return true;
        }
        if (args.length > 1 && args[0].equals("other-message")) {
            Player other = java.util.Objects.requireNonNull(Bukkit.getPlayer(java.util.UUID.fromString(args[1])));
            try { other.sendMessage("wrong recipient"); throw new AssertionError("message unexpectedly accepted"); }
            catch (UnsupportedOperationException expected) { sender.sendMessage("other message rejected"); }
            return true;
        }
        if (args.length > 1 && args[0].equals("lookup")) {
            Player match = Bukkit.getPlayer(args[1]);
            sender.sendMessage(match == null ? "missing" : match.getName()); return true;
        }
        if (args.length > 0 && args[0].equals("previous")) {
            sender.sendMessage(previous.isOnline() + ":" + previous.isValid() + ":" + previous.equals(player) + ":" + (previous == player) + ":" + previous.getEntityId()); return true;
        }
        World world = player.getWorld();
        Location first = player.getLocation();
        double original = first.getX();
        first.setX(original + 10);
        Location supplied = new Location(null, 0, 0, 0);
        if (player.getLocation(supplied) != supplied || player.getLocation(null) != null || player.getLocation().getX() != original) throw new AssertionError("location copy contract");
        if (Bukkit.getPlayer(player.getUniqueId()) != player || Bukkit.getPlayerExact(player.getName()) != player || !Bukkit.getOnlinePlayers().contains(player)) throw new AssertionError("player lookup identity");
        if (player.getPlayer() != player || player.hashCode() != 485 + player.getUniqueId().hashCode()) throw new AssertionError("Paper UUID identity contract");
        if (Bukkit.getWorld(world.getUID()) != world || Bukkit.getWorld(world.getName()) != world || Bukkit.getWorld(world.getKey()) != world || !Bukkit.getWorlds().contains(world)) throw new AssertionError("world lookup identity");
        Location spawn = world.getSpawnLocation();
        spawn.setY(999);
        if (world.getSpawnLocation().getY() == 999) throw new AssertionError("spawn leaked mutable state");
        boolean teleport = false;
        boolean write = false;
        try { player.teleport(player.getLocation()); } catch (UnsupportedOperationException expected) { teleport = true; }
        try { world.setTime(1); } catch (UnsupportedOperationException expected) { write = true; }
        if (!teleport || !write) throw new AssertionError("mutation was not explicitly rejected");
        sender.sendMessage(player.getUniqueId() + ":" + player.getEntityId() + ":" + player.getName() + ":" + player.isOnline() + ":" + player.isOp()
                + ":" + player.hasPermission("native.public") + ":" + player.hasPermission("native.operator") + ":" + world.getName()
                + ":" + world.getMinHeight() + ":" + world.getMaxHeight() + ":" + original);
        return true;
    }
}
