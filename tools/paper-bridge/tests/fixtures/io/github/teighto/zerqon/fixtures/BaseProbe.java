package io.github.teighto.zerqon.fixtures;

import org.bukkit.plugin.java.JavaPlugin;

public final class BaseProbe extends JavaPlugin {
    public static String sharedValue() { return "dependency-loaded"; }
}
