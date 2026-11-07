import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

public final class ReportCancellationProbe {
    public static void main(String[] args) throws Exception {
        Files.writeString(Path.of("generator.pid"), Long.toString(ProcessHandle.current().pid()), StandardCharsets.UTF_8);
        Files.writeString(Path.of("generator.classpath"), System.getProperty("java.class.path"), StandardCharsets.UTF_8);
        Files.writeString(Path.of("generator.ready"), "ready", StandardCharsets.UTF_8);
        Thread.sleep(60_000);
    }
}
