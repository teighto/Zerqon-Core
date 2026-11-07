package io.github.teighto.zerqon.paper;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import org.bukkit.Bukkit;
import org.bukkit.event.HandlerList;
import org.bukkit.plugin.java.JavaPlugin;

public final class PaperBridgeTest {
    private static int checks;
    private PaperBridgeTest() {}

    public static void main(String[] args) throws Exception {
        Path output = Path.of(args[0]);
        try (PaperBridge bridge = new PaperBridge(output)) {
            JavaPlugin plugin = bridge.load(Path.of(args[1]));
            check("true".equals(System.getProperty("zerqon.probe.loaded")), "onLoad invoked");
            bridge.enable(plugin);
            check(plugin.isEnabled(), "plugin enabled");
            check(Files.readString(plugin.getDataFolder().toPath().resolve("config.yml")).contains("enabled: true"), "real configuration persisted");
            check(bridge.execute("CONSOLE", false, false, "probe").equals(List.of("Permission denied")), "player named CONSOLE is not privileged");
            check(bridge.execute("Operator", true, false, "probe").equals(List.of("hello:0:0:Operator:probe")), "command executed with config");
            bridge.tick(1);
            bridge.tick(2);
            bridge.tick(3);
            check(bridge.execute("Operator", true, false, "p").equals(List.of("hello:2:1:Operator:p")), "scheduler and command alias work");
            java.util.concurrent.Future<String> cancelled = Bukkit.getScheduler().callSyncMethod(plugin, () -> "unexpected");
            Bukkit.getScheduler().cancelTasks(plugin);
            check(cancelled.isCancelled(), "cancelTasks cancels Future returned by callSyncMethod");
            Bukkit.getScheduler().runTaskTimer(plugin, () -> plugin.getConfig().set("tick-probe", true), 1, 2);
            bridge.tick(4);
            check(plugin.getConfig().getBoolean("tick-probe"), "new tasks execute after cancellation");
            check(bridge.execute("Operator", true, false, "probe event").equals(List.of("lowmonitor:true")), "event priority and cancellation honored");
            check(bridge.execute("CONSOLE", false, true, "paperprobe:probe").equals(List.of("hello:2:1:CONSOLE:paperprobe:probe")), "trusted console and namespaced command supported");
            boolean duplicate = false;
            try { bridge.load(Path.of(args[1])); } catch (IllegalArgumentException expected) { duplicate = true; }
            check(duplicate, "duplicate plugin is rejected");
            check(JavaPlugin.getProvidingPlugin(plugin.getClass()) == plugin, "configured classloader identifies owner");
            java.util.concurrent.atomic.AtomicReference<Throwable> threadFailure = new java.util.concurrent.atomic.AtomicReference<>();
            java.util.concurrent.atomic.AtomicBoolean correctlyReportsThread = new java.util.concurrent.atomic.AtomicBoolean();
            Thread thread = Thread.ofPlatform().start(() -> {
                try { correctlyReportsThread.set(!Bukkit.isPrimaryThread()); Bukkit.getScheduler().runTask(plugin, () -> {}); }
                catch (Throwable failure) { threadFailure.set(failure); }
            });
            thread.join();
            check(correctlyReportsThread.get(), "isPrimaryThread reports false on other threads");
            check(threadFailure.get() instanceof IllegalStateException, "wrong-thread API access rejected");
            boolean asyncUnsupported = false;
            try { Bukkit.getScheduler().runTaskAsynchronously(plugin, () -> {}); }
            catch (UnsupportedOperationException expected) { asyncUnsupported = expected.getMessage().contains("runTaskAsynchronously"); }
            check(asyncUnsupported, "unsupported async API is named in diagnostic");
            boolean lifecycleUnsupported = false;
            try { plugin.getLifecycleManager().registerEventHandler(new io.papermc.paper.plugin.lifecycle.event.handler.configuration.LifecycleEventHandlerConfiguration<org.bukkit.plugin.Plugin>() {}); }
            catch (UnsupportedOperationException expected) { lifecycleUnsupported = true; }
            check(lifecycleUnsupported, "unsupported Paper lifecycle registration fails explicitly");
            boolean unsupported = false;
            try { Bukkit.getWorlds(); } catch (UnsupportedOperationException expected) { unsupported = expected.getMessage().contains("getWorlds"); }
            check(unsupported, "unimplemented world API explicitly rejected");
            bridge.disable(plugin);
            check(!plugin.isEnabled() && "true".equals(System.getProperty("zerqon.probe.disabled")), "onDisable invoked");
            check(Bukkit.getScheduler().getPendingTasks().isEmpty(), "disabled plugin tasks removed");
            check(HandlerList.getRegisteredListeners(plugin).isEmpty(), "disabled plugin listeners removed");
            check(bridge.execute("Operator", true, false, "probe").equals(List.of("Unknown command")), "disabled plugin commands removed");

            JavaPlugin failing = bridge.load(Path.of(args[2]));
            boolean failed = false;
            try { bridge.enable(failing); } catch (RuntimeException expected) { failed = true; }
            check(failed && !failing.isEnabled(), "enable failure rolls back enabled state");
            bridge.tick(5);
            check(System.getProperty("zerqon.failed.task") == null, "failed enable tasks cancelled");
            check(Bukkit.getScheduler().getPendingTasks().isEmpty(), "failed plugin cleanup complete");
            boolean missingDependency = false;
            try { bridge.load(Path.of(args[4])); } catch (IllegalStateException expected) { missingDependency = expected.getMessage().contains("BaseProbe"); }
            check(missingDependency, "missing hard dependency is named and rejected");
            JavaPlugin base = bridge.load(Path.of(args[3]));
            bridge.enable(base);
            JavaPlugin dependent = bridge.load(Path.of(args[4]));
            bridge.enable(dependent);
            check("dependency-loaded".equals(System.getProperty("zerqon.dependency")), "dependency classes resolve from their actual owner JAR");
        }
        System.out.println("Paper bridge checks: " + checks + " passed");
    }

    private static void check(boolean result, String message) {
        if (!result) { throw new AssertionError(message); }
        checks++;
    }
}
