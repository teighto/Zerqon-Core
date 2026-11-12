import io.netty.buffer.ByteBuf;
import io.netty.buffer.Unpooled;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.HashSet;
import java.util.HexFormat;
import net.minecraft.world.phys.Vec3;

public final class LpVec3Oracle {
    private static String bits(double value) {
        return String.format("%016x", Double.doubleToRawLongBits(value));
    }

    private static double value(String bits) {
        if (!bits.matches("[0-9a-f]{16}")) throw new IllegalArgumentException("Invalid f64 bits");
        return Double.longBitsToDouble(Long.parseUnsignedLong(bits, 16));
    }

    private static Vec3 decode(byte[] bytes) {
        ByteBuf input = Unpooled.wrappedBuffer(bytes);
        try {
            Vec3 result = Vec3.LP_STREAM_CODEC.decode(input);
            if (input.isReadable()) throw new IllegalArgumentException("Trailing LP vector bytes");
            return result;
        } finally {
            input.release();
        }
    }

    public static void main(String[] args) throws Exception {
        Path input = Path.of(args[0]);
        if (Files.size(input) > 1_048_576) throw new IllegalArgumentException("Input exceeds 1 MiB");
        var result = new StringBuilder("# Minecraft 26.3 / protocol 777: original Vec3.LP_STREAM_CODEC\n");
        result.append("# name\tx_bits\ty_bits\tz_bits\twire_hex\tdecoded_x_bits\tdecoded_y_bits\tdecoded_z_bits\n");
        var names = new HashSet<String>();
        for (String line : Files.readAllLines(input, StandardCharsets.UTF_8)) {
            if (line.startsWith("#") || line.isBlank()) continue;
            String[] fields = line.split("\t", -1);
            if (fields.length != 4 || !fields[0].matches("[a-z0-9_]{1,80}") || !names.add(fields[0]) || names.size() > 2048) {
                throw new IllegalArgumentException("Invalid or duplicate LP fixture");
            }
            var vector = new Vec3(value(fields[1]), value(fields[2]), value(fields[3]));
            ByteBuf output = Unpooled.buffer();
            byte[] bytes;
            try {
                Vec3.LP_STREAM_CODEC.encode(output, vector);
                bytes = new byte[output.readableBytes()];
                output.readBytes(bytes);
            } finally {
                output.release();
            }
            if (bytes.length < 1 || bytes.length > 11) throw new AssertionError("Unexpected LP length");
            Vec3 decoded = decode(bytes);
            for (int size = 0; size < bytes.length; size++) {
                try {
                    decode(Arrays.copyOf(bytes, size));
                } catch (RuntimeException expected) {
                    continue;
                }
                throw new AssertionError("Truncated LP fixture was accepted: " + fields[0]);
            }
            try {
                decode(Arrays.copyOf(bytes, bytes.length + 1));
                throw new AssertionError("LP exhaustion check failed");
            } catch (IllegalArgumentException expected) {
                // The wrapper, not the original stream codec, requires exhaustion.
            }
            result.append(line).append('\t').append(HexFormat.of().formatHex(bytes))
                .append('\t').append(bits(decoded.x)).append('\t').append(bits(decoded.y))
                .append('\t').append(bits(decoded.z)).append('\n');
        }
        if (names.isEmpty()) throw new IllegalArgumentException("No LP fixtures");
        Files.writeString(Path.of(args[1]), result, StandardCharsets.UTF_8);
        System.out.println("Original LP Vec3 codec checked " + names.size() + " vectors");
    }
}
