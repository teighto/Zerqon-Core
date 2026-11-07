import java.nio.channels.FileChannel;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

class SessionLock {
    public static void main(String[] args) throws Exception {
        try (var channel = FileChannel.open(Path.of(args[0]), StandardOpenOption.READ, StandardOpenOption.WRITE);
             var lock = channel.tryLock()) {
            if (lock == null) throw new IllegalStateException("lock already held");
            System.out.println("locked");
            System.out.flush();
            System.in.read();
        }
    }
}
