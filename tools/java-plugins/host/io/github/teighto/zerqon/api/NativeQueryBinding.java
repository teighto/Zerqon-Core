package io.github.teighto.zerqon.api;

import java.util.Objects;

public final class NativeQueryBinding {
    private NativeQueryBinding() {}

    public static Scope bind(NativeQueries.Access query) {
        Objects.requireNonNull(query, "query");
        Thread owner = Thread.currentThread();
        NativeQueries.Access previous = NativeQueries.CURRENT.get();
        NativeQueries.CURRENT.set(query);
        return () -> {
            if (Thread.currentThread() != owner || NativeQueries.CURRENT.get() != query) throw new IllegalStateException("Native callback scopes must close on their owner in reverse order");
            if (previous == null) NativeQueries.CURRENT.remove(); else NativeQueries.CURRENT.set(previous);
        };
    }

    @FunctionalInterface
    public interface Scope extends AutoCloseable {
        @Override void close();
    }
}
