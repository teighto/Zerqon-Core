package io.github.teighto.zerqon.fixtures;

import org.bukkit.plugin.java.JavaPlugin;

public final class DependentProbe extends JavaPlugin {
    @Override public void onEnable() { System.setProperty("zerqon.dependency", BaseProbe.sharedValue()); }
}
