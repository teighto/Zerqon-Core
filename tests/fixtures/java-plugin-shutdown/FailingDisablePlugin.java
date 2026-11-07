import io.github.teighto.zerqon.api.Action;
import io.github.teighto.zerqon.api.Event;
import io.github.teighto.zerqon.api.Plugin;
import java.util.List;

public final class FailingDisablePlugin implements Plugin {
    @Override
    public List<Action> onEvent(Event event) {
        return switch (event) {
            case Event.Enable ignored -> List.of(new Action.Log(Action.Level.INFO,
                    "SHUTDOWN_FIXTURE_PID=" + ProcessHandle.current().pid()));
            case Event.Disable ignored -> throw new IllegalStateException("deliberate Java Disable failure");
            default -> List.of();
        };
    }
}
