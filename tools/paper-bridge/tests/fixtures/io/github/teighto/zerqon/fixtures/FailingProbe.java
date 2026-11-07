package io.github.teighto.zerqon.fixtures;

import org.bukkit.Bukkit;
import org.bukkit.plugin.java.JavaPlugin;

public final class FailingProbe extends JavaPlugin {
    @Override public void onEnable() {
        Bukkit.getScheduler().runTask(this, () -> System.setProperty("zerqon.failed.task", "ran"));
        throw new IllegalStateException("intentional enable failure");
    }
}
