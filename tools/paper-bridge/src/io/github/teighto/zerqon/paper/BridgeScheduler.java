package io.github.teighto.zerqon.paper;

import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Callable;
import java.util.concurrent.Executor;
import java.util.concurrent.FutureTask;
import java.util.function.Consumer;
import org.bukkit.plugin.Plugin;
import org.bukkit.scheduler.BukkitScheduler;
import org.bukkit.scheduler.BukkitTask;

final class BridgeScheduler {
    private final PaperBridge bridge;
    private final Map<Integer, Task> tasks = new LinkedHashMap<>();
    private final BukkitScheduler api = PaperBridge.proxy(BukkitScheduler.class, this::invoke);
    private long tick;
    private int sequence;
    private int running = -1;

    BridgeScheduler(PaperBridge bridge) { this.bridge = bridge; }
    BukkitScheduler api() { return api; }
    long currentTick() { return tick; }
    void cancel(Plugin plugin) { for (Task task : new ArrayList<>(tasks.values())) { if (task.owner == plugin) { task.cancel(); } } }

    void tick(long value) {
        if (value <= tick) { throw new IllegalArgumentException("Ticks must increase"); }
        tick = value;
        for (Task task : new ArrayList<>(tasks.values())) {
            if (task.cancelled || task.due > tick) { continue; }
            if (task.period == 0) { tasks.remove(task.id); } else { task.due = Math.addExact(tick, task.period); }
            running = task.id;
            try { task.run(); }
            catch (RuntimeException failure) {
                task.cancel();
                task.owner.getLogger().log(java.util.logging.Level.SEVERE, "Scheduled task failed", failure);
            } finally { running = -1; }
        }
    }

    private Task schedule(Plugin owner, Object work, long delay, long period) {
        if (!owner.isEnabled()) { throw new IllegalStateException("Cannot schedule a disabled plugin"); }
        if (tasks.size() >= 4096) { throw new IllegalStateException("Pending task limit is 4096"); }
        if (!(work instanceof Runnable) && !(work instanceof Consumer<?>)) { throw new IllegalArgumentException("Task must be Runnable or Consumer"); }
        Task task = new Task(Math.incrementExact(sequence), owner, work, Math.addExact(tick, Math.max(1, delay)), period);
        sequence = task.id;
        tasks.put(task.id, task);
        return task;
    }

    private Object invoke(Object object, Method method, Object[] args) {
        bridge.checkThread();
        String name = method.getName();
        if (name.contains("Async")) { throw PaperBridge.unsupported(method); }
        return switch (name) {
            case "runTask", "runTaskLater", "runTaskTimer", "scheduleSyncDelayedTask", "scheduleSyncRepeatingTask" -> {
                long delay = args.length > 2 ? (Long) args[2] : 0;
                long period = args.length > 3 ? (Long) args[3] : 0;
                if (args.length > 3 && period <= 0) { throw new IllegalArgumentException("Repeating period must be positive"); }
                Task task = schedule((Plugin) args[0], args[1], delay, period);
                yield method.getReturnType() == int.class ? task.id : method.getReturnType() == void.class ? null : task;
            }
            case "cancelTask" -> { Task task = tasks.get((Integer) args[0]); if (task != null) { task.cancel(); } yield null; }
            case "cancelTasks" -> { cancel((Plugin) args[0]); yield null; }
            case "isQueued" -> tasks.containsKey((Integer) args[0]);
            case "isCurrentlyRunning" -> running == (Integer) args[0];
            case "getPendingTasks" -> List.copyOf(tasks.values());
            case "getActiveWorkers" -> List.of();
            case "getMainThreadExecutor" -> (Executor) work -> { bridge.checkThread(); schedule((Plugin) args[0], work, 0, 0); };
            case "callSyncMethod" -> { FutureTask<?> task = new FutureTask<>((Callable<?>) args[1]); schedule((Plugin) args[0], task, 0, 0); yield task; }
            default -> throw PaperBridge.unsupported(method);
        };
    }

    private final class Task implements BukkitTask {
        private final int id;
        private final Plugin owner;
        private final Object work;
        private final long period;
        private long due;
        private boolean cancelled;
        Task(int id, Plugin owner, Object work, long due, long period) { this.id=id; this.owner=owner; this.work=work; this.due=due; this.period=period; }
        @SuppressWarnings("unchecked")
        void run() { PaperBridge.context(owner, () -> { if (work instanceof Runnable runnable) { runnable.run(); } else { ((Consumer<BukkitTask>) work).accept(this); } return null; }); }
        @Override public int getTaskId() { return id; }
        @Override public Plugin getOwner() { return owner; }
        @Override public boolean isSync() { return true; }
        @Override public boolean isCancelled() { return cancelled; }
        @Override public void cancel() { bridge.checkThread(); cancelled = true; tasks.remove(id); if (work instanceof FutureTask<?> future) { future.cancel(false); } }
    }
}
