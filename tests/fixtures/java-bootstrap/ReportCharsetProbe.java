import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

public final class ReportCharsetProbe {
    public static void main(String[] args) throws Exception {
        if (!System.out.charset().equals(StandardCharsets.UTF_8)
                || !System.err.charset().equals(StandardCharsets.UTF_8)) {
            throw new IllegalStateException("Generator stdout and stderr must both be UTF-8");
        }
        Path output = null;
        for (int i = 0; i + 1 < args.length; i++) {
            if (args[i].equals("--output")) output = Path.of(args[++i]);
        }
        if (output == null) throw new IllegalArgumentException("missing output");
        Files.createDirectories(output);
        System.out.println("Генератор: стандартный вывод");
        System.err.println("Генератор: вывод ошибок");
        java.util.logging.Logger.getLogger("generator").info("Генератор: журнал JUL");
        Files.writeString(output.resolve("charset-result.txt"), "Привет из генератора", StandardCharsets.UTF_8);
    }
}
