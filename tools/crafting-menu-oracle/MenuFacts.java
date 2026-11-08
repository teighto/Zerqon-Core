import com.google.gson.GsonBuilder;
import com.google.gson.JsonNull;
import com.google.gson.JsonObject;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;
import net.minecraft.SharedConstants;
import net.minecraft.core.component.DataComponentInitializers;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.data.registries.VanillaRegistries;
import net.minecraft.server.Bootstrap;

/** Reads wire item identities from the pinned original classes, not cached reports. */
public final class MenuFacts {
    public static void main(String[] args) throws Exception {
        if (args.length != 1) throw new IllegalArgumentException("Expected output JSON path");
        SharedConstants.tryDetectVersion();
        Bootstrap.bootStrap();
        BuiltInRegistries.DATA_COMPONENT_INITIALIZERS.build(VanillaRegistries.createWorldLookup())
            .forEach(DataComponentInitializers.PendingComponents::apply);
        var version = SharedConstants.getCurrentVersion();
        JsonObject output = new JsonObject();
        output.addProperty("minecraft", version.id());
        output.addProperty("protocol", version.protocolVersion());
        output.addProperty("data_version", version.dataVersion().version());
        JsonObject items = new JsonObject();
        BuiltInRegistries.ITEM.stream().sorted(Comparator.comparing(item -> BuiltInRegistries.ITEM.getKey(item).toString()))
            .forEach(item -> {
                JsonObject value = new JsonObject();
                var stack = item.getDefaultInstance();
                value.addProperty("id", BuiltInRegistries.ITEM.getId(item));
                value.addProperty("max_stack", stack.getMaxStackSize());
                if (stack.getMaxDamage() == 0) value.add("max_damage", JsonNull.INSTANCE);
                else value.addProperty("max_damage", stack.getMaxDamage());
                items.add(BuiltInRegistries.ITEM.getKey(item).toString(), value);
            });
        output.add("items", items);
        Files.writeString(Path.of(args[0]), new GsonBuilder().serializeNulls().create().toJson(output), StandardCharsets.UTF_8);
    }
}
