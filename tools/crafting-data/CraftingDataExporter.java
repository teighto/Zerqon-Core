import com.google.gson.GsonBuilder;
import com.google.gson.JsonArray;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;
import java.util.zip.ZipFile;
import net.minecraft.SharedConstants;
import net.minecraft.core.HolderLookup;
import net.minecraft.core.component.DataComponentInitializers;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.data.registries.VanillaRegistries;
import net.minecraft.server.Bootstrap;

/** Queries original public methods; no Minecraft implementation is reproduced here. */
public final class CraftingDataExporter {
    static final int MAX_JSON_BYTES = 1_048_576;

    static void require(boolean condition, String message) {
        if (!condition) throw new IllegalArgumentException(message);
    }

    static HolderLookup.Provider bootstrap() {
        SharedConstants.tryDetectVersion();
        Bootstrap.bootStrap();
        var lookup = VanillaRegistries.createWorldLookup();
        BuiltInRegistries.DATA_COMPONENT_INITIALIZERS.build(lookup)
            .forEach(DataComponentInitializers.PendingComponents::apply);
        return lookup;
    }

    static JsonObject json(Path path) throws Exception {
        require(Files.size(path) <= MAX_JSON_BYTES, "JSON input exceeds 1 MiB: " + path);
        return JsonParser.parseString(Files.readString(path, StandardCharsets.UTF_8)).getAsJsonObject();
    }

    static JsonObject json(ZipFile jar, String path) throws Exception {
        var entry = jar.getEntry(path);
        require(entry != null && !entry.isDirectory(), "Missing original JSON: " + path);
        require(entry.getSize() >= 0 && entry.getSize() <= MAX_JSON_BYTES, "Original JSON exceeds 1 MiB: " + path);
        try (var input = jar.getInputStream(entry)) {
            byte[] bytes = input.readNBytes(MAX_JSON_BYTES + 1);
            require(bytes.length <= MAX_JSON_BYTES, "Original JSON exceeds 1 MiB: " + path);
            return JsonParser.parseString(new String(bytes, StandardCharsets.UTF_8)).getAsJsonObject();
        }
    }

    static JsonObject facts() {
        JsonArray remainders = new JsonArray();
        var items = BuiltInRegistries.ITEM.stream()
            .sorted(Comparator.comparing(item -> BuiltInRegistries.ITEM.getKey(item).toString())).toList();
        require(items.size() == 1658, "Unexpected original item count");
        for (var item : items) {
            var remainder = item.getCraftingRemainder();
            if (remainder == null) continue;
            var stack = remainder.create();
            String key = BuiltInRegistries.ITEM.getKey(item).toString();
            require(!stack.isEmpty(), "Empty crafting remainder for " + key);
            require(stack.getComponentsPatch().isEmpty(), "Schema 1 cannot encode component-bearing remainder: " + key);
            JsonArray row = new JsonArray();
            row.add(key);
            row.add(BuiltInRegistries.ITEM.getKey(stack.getItem()).toString());
            row.add(stack.getCount());
            remainders.add(row);
        }
        JsonObject result = new JsonObject();
        var version = SharedConstants.getCurrentVersion();
        result.addProperty("minecraft", version.id());
        result.addProperty("protocol", version.protocolVersion());
        result.addProperty("data_version", version.dataVersion().version());
        result.addProperty("item_count", items.size());
        result.add("remainders", remainders);
        return result;
    }

    public static void main(String[] args) throws Exception {
        require(args.length == 4, "Expected server.jar fixtures.json facts.json report.json");
        var lookup = bootstrap();
        JsonObject facts = facts();
        JsonObject report = CraftingRecipeOracle.verify(Path.of(args[0]), Path.of(args[1]), lookup);
        var gson = new GsonBuilder().serializeNulls().create();
        Files.writeString(Path.of(args[2]), gson.toJson(facts), StandardCharsets.UTF_8);
        Files.writeString(Path.of(args[3]), gson.toJson(report), StandardCharsets.UTF_8);
    }
}
