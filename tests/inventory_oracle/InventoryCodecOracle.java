import com.google.gson.GsonBuilder;
import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonNull;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import com.mojang.serialization.JsonOps;
import com.mojang.serialization.Lifecycle;
import com.mojang.datafixers.util.Pair;
import io.netty.buffer.Unpooled;
import it.unimi.dsi.fastutil.ints.Int2ObjectOpenHashMap;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import java.util.Set;
import net.minecraft.SharedConstants;
import net.minecraft.core.BlockPos;
import net.minecraft.core.Direction;
import net.minecraft.core.HolderLookup;
import net.minecraft.core.MappedRegistry;
import net.minecraft.core.RegistrationInfo;
import net.minecraft.core.Registry;
import net.minecraft.core.RegistryAccess;
import net.minecraft.core.component.DataComponentInitializers;
import net.minecraft.core.component.DataComponents;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.data.registries.VanillaRegistries;
import net.minecraft.network.HashedPatchMap;
import net.minecraft.network.HashedStack;
import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.chat.Component;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.game.ClientboundContainerSetContentPacket;
import net.minecraft.network.protocol.game.ClientboundContainerSetSlotPacket;
import net.minecraft.network.protocol.game.ClientboundSetCursorItemPacket;
import net.minecraft.network.protocol.game.ClientboundSetEquipmentPacket;
import net.minecraft.network.protocol.game.ClientboundSetHeldSlotPacket;
import net.minecraft.network.protocol.game.ClientboundSetPlayerInventoryPacket;
import net.minecraft.network.protocol.game.ServerboundContainerClickPacket;
import net.minecraft.network.protocol.game.ServerboundSetCarriedItemPacket;
import net.minecraft.network.protocol.game.ServerboundSetCreativeModeSlotPacket;
import net.minecraft.network.protocol.game.ServerboundUseItemOnPacket;
import net.minecraft.resources.Identifier;
import net.minecraft.resources.ResourceKey;
import net.minecraft.server.Bootstrap;
import net.minecraft.world.InteractionHand;
import net.minecraft.world.entity.EquipmentSlot;
import net.minecraft.world.inventory.ContainerInput;
import net.minecraft.world.item.ItemStack;
import net.minecraft.world.item.Items;
import net.minecraft.world.item.Rarity;
import net.minecraft.world.item.component.BlockItemStateProperties;
import net.minecraft.world.phys.BlockHitResult;
import net.minecraft.world.phys.Vec3;

public final class InventoryCodecOracle {
    private static final int MAX_FIXTURE_BYTES = 1_048_576;
    private static final int MAX_JSON_BYTES = 16_777_216;
    private static final Set<String> KINDS = Set.of("creative_slot", "container_click", "carried_slot", "use_item_on", "server_stack", "content", "slot", "cursor", "held_slot", "player_inventory", "equipment");
    private final RegistryAccess registries;

    private InventoryCodecOracle(RegistryAccess registries) { this.registries = registries; }

    private static void require(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }

    private static JsonObject json(Path path) throws Exception {
        require(Files.size(path) <= MAX_JSON_BYTES, "JSON input exceeds 16 MiB: " + path);
        return JsonParser.parseString(Files.readString(path, StandardCharsets.UTF_8)).getAsJsonObject();
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    private static Registry<?> registry(HolderLookup.Provider source, JsonObject definition) {
        ResourceKey key = ResourceKey.createRegistryKey(Identifier.parse(definition.get("name").getAsString()));
        HolderLookup.RegistryLookup lookup = source.lookupOrThrow(key);
        MappedRegistry result = new MappedRegistry(key, Lifecycle.stable());
        for (var entry : definition.getAsJsonArray("entries")) {
            ResourceKey entryKey = ResourceKey.create(key, Identifier.parse(entry.getAsJsonObject().get("id").getAsString()));
            var holder = lookup.getOrThrow(entryKey);
            result.register(entryKey, holder.value(), RegistrationInfo.BUILT_IN);
        }
        result.bindAllTagsToEmpty();
        return result.freeze();
    }

    private static RegistryAccess registries(Path data) throws Exception {
        SharedConstants.tryDetectVersion();
        Bootstrap.bootStrap();
        var source = VanillaRegistries.createWorldLookup();
        List<Registry<?>> entries = new ArrayList<>();
        BuiltInRegistries.REGISTRY.forEach(entries::add);
        for (var definition : json(data.resolve("registries-sync.json")).getAsJsonArray("registries")) {
            entries.add(registry(source, definition.getAsJsonObject()));
        }
        BuiltInRegistries.DATA_COMPONENT_INITIALIZERS.build(source).forEach(DataComponentInitializers.PendingComponents::apply);
        return new RegistryAccess.ImmutableRegistryAccess(entries).freeze();
    }

    private static void packetIds(Path data) throws Exception {
        JsonObject play = json(data.resolve("generated/reports/packets.json")).getAsJsonObject("play");
        Map<String, Integer> clientbound = Map.of("container_set_content", 18, "container_set_slot", 20, "set_cursor_item", 98, "set_equipment", 104, "set_held_slot", 107, "set_player_inventory", 110);
        Map<String, Integer> serverbound = Map.of("set_creative_mode_slot", 57, "set_carried_item", 54, "container_click", 18, "use_item_on", 66);
        for (var direction : Map.of("clientbound", clientbound, "serverbound", serverbound).entrySet()) {
            for (var packet : direction.getValue().entrySet()) {
                int actual = play.getAsJsonObject(direction.getKey()).getAsJsonObject("minecraft:" + packet.getKey()).get("protocol_id").getAsInt();
                require(actual == packet.getValue(), "Wrong 26.3 packet ID: " + packet.getKey());
            }
        }
    }

    private <T> byte[] encode(StreamCodec<? super RegistryFriendlyByteBuf, T> codec, T value) {
        var buffer = new RegistryFriendlyByteBuf(Unpooled.buffer(), registries);
        try {
            codec.encode(buffer, value);
            byte[] result = new byte[buffer.readableBytes()];
            buffer.readBytes(result);
            return result;
        } finally { buffer.release(); }
    }

    private Object decode(String kind, byte[] bytes) {
        var buffer = new RegistryFriendlyByteBuf(Unpooled.wrappedBuffer(bytes), registries);
        try {
            Object result = switch (kind) {
                case "creative_slot" -> ServerboundSetCreativeModeSlotPacket.STREAM_CODEC.decode(buffer);
                case "container_click" -> ServerboundContainerClickPacket.STREAM_CODEC.decode(buffer);
                case "carried_slot" -> ServerboundSetCarriedItemPacket.STREAM_CODEC.decode(buffer);
                case "use_item_on" -> ServerboundUseItemOnPacket.STREAM_CODEC.decode(buffer);
                case "server_stack" -> ItemStack.OPTIONAL_STREAM_CODEC.decode(buffer);
                case "content" -> ClientboundContainerSetContentPacket.STREAM_CODEC.decode(buffer);
                case "slot" -> ClientboundContainerSetSlotPacket.STREAM_CODEC.decode(buffer);
                case "cursor" -> ClientboundSetCursorItemPacket.STREAM_CODEC.decode(buffer);
                case "held_slot" -> ClientboundSetHeldSlotPacket.STREAM_CODEC.decode(buffer);
                case "player_inventory" -> ClientboundSetPlayerInventoryPacket.STREAM_CODEC.decode(buffer);
                case "equipment" -> ClientboundSetEquipmentPacket.STREAM_CODEC.decode(buffer);
                default -> throw new IllegalArgumentException("Unknown fixture kind: " + kind);
            };
            require(!buffer.isReadable(), "Trailing packet bytes for " + kind + ": " + buffer.readableBytes());
            return result;
        } finally { buffer.release(); }
    }

    private JsonElement stack(ItemStack item) {
        if (item.isEmpty()) return JsonNull.INSTANCE;
        JsonObject result = new JsonObject();
        result.addProperty("item", BuiltInRegistries.ITEM.getKey(item.getItem()).toString());
        result.addProperty("count", item.getCount());
        JsonObject added = new JsonObject();
        var patch = item.getComponentsPatch().split();
        patch.added().stream().sorted(java.util.Comparator.comparing(component -> BuiltInRegistries.DATA_COMPONENT_TYPE.getKey(component.type()).toString()))
            .forEach(component -> added.add(BuiltInRegistries.DATA_COMPONENT_TYPE.getKey(component.type()).toString(), component.encodeValue(registries.createSerializationContext(JsonOps.INSTANCE)).getOrThrow()));
        result.add("added", added);
        JsonArray removed = new JsonArray();
        patch.removed().stream().map(type -> BuiltInRegistries.DATA_COMPONENT_TYPE.getKey(type).toString()).sorted().forEach(removed::add);
        result.add("removed", removed);
        return result;
    }

    private JsonElement hashed(HashedStack stack) {
        if (!(stack instanceof HashedStack.ActualItem item)) return JsonNull.INSTANCE;
        JsonObject result = new JsonObject();
        result.addProperty("item", BuiltInRegistries.ITEM.getKey(item.item().value()).toString());
        result.addProperty("count", item.count());
        JsonObject added = new JsonObject();
        item.components().addedComponents().entrySet().stream().sorted(java.util.Comparator.comparing(entry -> BuiltInRegistries.DATA_COMPONENT_TYPE.getKey(entry.getKey()).toString()))
            .forEach(entry -> added.addProperty(BuiltInRegistries.DATA_COMPONENT_TYPE.getKey(entry.getKey()).toString(), entry.getValue()));
        result.add("added", added);
        JsonArray removed = new JsonArray();
        item.components().removedComponents().stream().map(type -> BuiltInRegistries.DATA_COMPONENT_TYPE.getKey(type).toString()).sorted().forEach(removed::add);
        result.add("removed", removed);
        return result;
    }

    private JsonElement normalized(Object value) {
        if (value instanceof ItemStack item) return stack(item);
        JsonObject result = new JsonObject();
        switch (value) {
            case ServerboundSetCreativeModeSlotPacket packet -> {
                result.addProperty("slot", packet.slotNum()); result.add("stack", stack(packet.itemStack()));
            }
            case ServerboundSetCarriedItemPacket packet -> result.addProperty("slot", packet.getSlot());
            case ClientboundSetHeldSlotPacket packet -> result.addProperty("slot", packet.slot());
            case ClientboundSetCursorItemPacket packet -> result.add("stack", stack(packet.contents()));
            case ClientboundSetPlayerInventoryPacket packet -> {
                result.addProperty("slot", packet.slot()); result.add("stack", stack(packet.contents()));
            }
            case ClientboundSetEquipmentPacket packet -> {
                result.addProperty("entity_id", packet.getEntity());
                JsonArray slots = new JsonArray();
                for (var entry : packet.getSlots()) {
                    JsonObject slot = new JsonObject(); slot.addProperty("slot", entry.getFirst().ordinal()); slot.add("stack", stack(entry.getSecond())); slots.add(slot);
                }
                result.add("slots", slots);
            }
            case ClientboundContainerSetSlotPacket packet -> {
                result.addProperty("container_id", packet.getContainerId()); result.addProperty("state_id", packet.getStateId());
                result.addProperty("slot", packet.getSlot()); result.add("stack", stack(packet.getItem()));
            }
            case ClientboundContainerSetContentPacket packet -> {
                result.addProperty("container_id", packet.containerId()); result.addProperty("state_id", packet.stateId());
                JsonArray items = new JsonArray(); packet.items().forEach(item -> items.add(stack(item))); result.add("items", items);
                result.add("carried", stack(packet.carriedItem()));
            }
            case ServerboundContainerClickPacket packet -> {
                result.addProperty("container_id", packet.containerId()); result.addProperty("state_id", packet.stateId());
                result.addProperty("slot", packet.slotNum()); result.addProperty("button", packet.buttonNum()); result.addProperty("mode", packet.containerInput().id());
                JsonArray changed = new JsonArray();
                packet.changedSlots().int2ObjectEntrySet().stream().sorted(java.util.Comparator.comparingInt(entry -> entry.getIntKey())).forEach(entry -> {
                    JsonObject slot = new JsonObject(); slot.addProperty("slot", entry.getIntKey()); slot.add("stack", hashed(entry.getValue())); changed.add(slot);
                });
                result.add("changed_slots", changed); result.add("carried", hashed(packet.carriedItem()));
            }
            case ServerboundUseItemOnPacket packet -> {
                result.addProperty("hand", packet.hand().ordinal());
                BlockHitResult hit = packet.hitResult(); BlockPos position = hit.getBlockPos(); Vec3 location = hit.getLocation();
                JsonArray block = new JsonArray(); block.add(position.getX()); block.add(position.getY()); block.add(position.getZ()); result.add("position", block);
                result.addProperty("face", hit.getDirection().ordinal());
                JsonArray local = new JsonArray(); local.add(location.x() - position.getX()); local.add(location.y() - position.getY()); local.add(location.z() - position.getZ()); result.add("hit", local);
                result.addProperty("inside", hit.isInside()); result.addProperty("world_border_hit", hit.isWorldBorderHit()); result.addProperty("sequence", packet.sequence());
            }
            default -> throw new IllegalArgumentException("Unsupported decoded type " + value.getClass().getName());
        }
        return result;
    }

    private int check(JsonObject document) {
        require(document.get("version").getAsString().equals("26.3") && document.get("protocol").getAsInt() == 777, "Wrong fixture protocol identity");
        JsonArray fixtures = document.getAsJsonArray("fixtures");
        require(!fixtures.isEmpty() && fixtures.size() <= 10000, "Expected 1..10000 fixtures");
        Set<String> names = new java.util.HashSet<>();
        for (JsonElement entry : fixtures) {
            JsonObject fixture = entry.getAsJsonObject(); String name = fixture.get("name").getAsString(); String kind = fixture.get("kind").getAsString();
            require(names.add(name), "Duplicate fixture " + name);
            require(KINDS.contains(kind), "Unknown fixture kind: " + kind);
            String hex = fixture.get("body_hex").getAsString();
            require(hex.length() <= MAX_FIXTURE_BYTES * 2, "Fixture body exceeds limit: " + name);
            byte[] bytes = HexFormat.of().parseHex(hex);
            boolean expectError = fixture.has("expect_error") && fixture.get("expect_error").getAsBoolean();
            Object decoded;
            try { decoded = decode(kind, bytes); }
            catch (RuntimeException failure) {
                if (expectError) continue;
                throw new AssertionError("Official codec rejected " + name, failure);
            }
            require(!expectError, "Official codec unexpectedly accepted " + name);
            JsonElement actual = normalized(decoded);
            require(actual.equals(fixture.get("expected")), "Decoded value differs for " + name + "\nExpected: " + fixture.get("expected") + "\nActual: " + actual);
        }
        return fixtures.size();
    }

    private <T> void fixture(JsonArray output, String name, String kind, StreamCodec<? super RegistryFriendlyByteBuf, T> codec, T value) {
        byte[] bytes = encode(codec, value);
        JsonObject fixture = new JsonObject(); fixture.addProperty("name", name); fixture.addProperty("kind", kind);
        fixture.addProperty("body_hex", HexFormat.of().formatHex(bytes)); fixture.add("expected", normalized(value));
        require(normalized(decode(kind, bytes)).equals(fixture.get("expected")), "Official encode/decode disagrees for " + name);
        output.add(fixture);
    }

    private void stackFixtures(JsonArray output, String name, ItemStack item) {
        fixture(output, "creative_" + name, "creative_slot", ServerboundSetCreativeModeSlotPacket.STREAM_CODEC, new ServerboundSetCreativeModeSlotPacket(36, item));
        fixture(output, "server_" + name, "server_stack", ItemStack.OPTIONAL_STREAM_CODEC, item);
    }

    private JsonObject export(boolean defaults) {
        JsonObject document = new JsonObject(); document.addProperty("version", "26.3"); document.addProperty("protocol", 777);
        JsonArray output = new JsonArray(); document.add("fixtures", output);
        ItemStack stone = new ItemStack(Items.STONE, 64);
        stackFixtures(output, "empty", ItemStack.EMPTY);
        stackFixtures(output, "stone64", stone);
        ItemStack named = new ItemStack(Items.STONE, 1); named.set(DataComponents.CUSTOM_NAME, Component.literal("Safe")); stackFixtures(output, "custom_name", named);
        ItemStack unicode = new ItemStack(Items.STONE, 1); unicode.set(DataComponents.CUSTOM_NAME, Component.literal("Привет 😀")); stackFixtures(output, "unicode_custom_name", unicode);
        ItemStack itemName = new ItemStack(Items.STONE, 1); itemName.set(DataComponents.ITEM_NAME, Component.literal("Name")); stackFixtures(output, "item_name", itemName);
        ItemStack maximum = new ItemStack(Items.STONE, 99); maximum.set(DataComponents.MAX_STACK_SIZE, 99); stackFixtures(output, "max_stack99", maximum);
        ItemStack maxDamage = new ItemStack(Items.DIAMOND_SWORD, 1); maxDamage.set(DataComponents.MAX_DAMAGE, 1000); stackFixtures(output, "max_damage1000", maxDamage);
        ItemStack damage = new ItemStack(Items.DIAMOND_SWORD, 1); damage.set(DataComponents.DAMAGE, 7); stackFixtures(output, "damage7", damage);
        ItemStack repair = new ItemStack(Items.DIAMOND_SWORD, 1); repair.set(DataComponents.REPAIR_COST, 12); stackFixtures(output, "repair_cost12", repair);
        ItemStack oriented = new ItemStack(Items.OAK_LOG, 1); oriented.set(DataComponents.BLOCK_STATE, new BlockItemStateProperties(Map.of("axis", "x"))); stackFixtures(output, "block_state", oriented);
        ItemStack model = new ItemStack(Items.STONE, 1); model.set(DataComponents.ITEM_MODEL, Identifier.parse("minecraft:diamond")); stackFixtures(output, "item_model", model);
        ItemStack rarity = new ItemStack(Items.STONE, 1); rarity.set(DataComponents.RARITY, Rarity.EPIC); stackFixtures(output, "rarity_epic", rarity);
        ItemStack removed = new ItemStack(Items.STONE, 1); removed.remove(DataComponents.REPAIR_COST); stackFixtures(output, "removed_repair_cost", removed);
        fixture(output, "slot_named", "slot", ClientboundContainerSetSlotPacket.STREAM_CODEC, new ClientboundContainerSetSlotPacket(0, 7, 36, named));
        fixture(output, "content", "content", ClientboundContainerSetContentPacket.STREAM_CODEC, new ClientboundContainerSetContentPacket(0, 7, List.of(stone, ItemStack.EMPTY), named));
        fixture(output, "cursor", "cursor", ClientboundSetCursorItemPacket.STREAM_CODEC, new ClientboundSetCursorItemPacket(named));
        fixture(output, "player_inventory", "player_inventory", ClientboundSetPlayerInventoryPacket.STREAM_CODEC, new ClientboundSetPlayerInventoryPacket(0, stone));
        fixture(output, "carried8", "carried_slot", ServerboundSetCarriedItemPacket.STREAM_CODEC, new ServerboundSetCarriedItemPacket(8));
        fixture(output, "held8", "held_slot", ClientboundSetHeldSlotPacket.STREAM_CODEC, new ClientboundSetHeldSlotPacket(8));
        fixture(output, "use_on", "use_item_on", ServerboundUseItemOnPacket.STREAM_CODEC, new ServerboundUseItemOnPacket(InteractionHand.MAIN_HAND, new BlockHitResult(new Vec3(2.25, 64.5, -2.25), Direction.UP, new BlockPos(2, 64, -3), false), 17));
        var predicted = new HashedStack.ActualItem(stone.typeHolder(), 64, new HashedPatchMap(Map.of(DataComponents.MAX_STACK_SIZE, 0x12345678), Set.of(DataComponents.CUSTOM_NAME)));
        var changed = new Int2ObjectOpenHashMap<HashedStack>(); changed.put(36, predicted);
        fixture(output, "click_hashed", "container_click", ServerboundContainerClickPacket.STREAM_CODEC, new ServerboundContainerClickPacket(0, 7, (short) 36, (byte) 0, ContainerInput.PICKUP, changed, HashedStack.EMPTY));
        if (defaults) {
            for (var item : BuiltInRegistries.ITEM) {
                String name = BuiltInRegistries.ITEM.getKey(item).toString();
                fixture(output, "default/" + name, "server_stack", ItemStack.OPTIONAL_STREAM_CODEC, new ItemStack(item, 1));
            }
        }
        return document;
    }

    private JsonObject equipmentExamples() {
        JsonObject document = new JsonObject(); document.addProperty("version", "26.3"); document.addProperty("protocol", 777);
        JsonArray output = new JsonArray(); document.add("fixtures", output);
        ItemStack stone = new ItemStack(Items.STONE, 64);
        ItemStack named = new ItemStack(Items.STONE, 1); named.set(DataComponents.CUSTOM_NAME, Component.literal("Safe"));
        fixture(output, "equipment_empty_hands", "equipment", ClientboundSetEquipmentPacket.STREAM_CODEC, new ClientboundSetEquipmentPacket(7, List.of(Pair.of(EquipmentSlot.MAINHAND, ItemStack.EMPTY), Pair.of(EquipmentSlot.OFFHAND, ItemStack.EMPTY))));
        fixture(output, "equipment_main_stone", "equipment", ClientboundSetEquipmentPacket.STREAM_CODEC, new ClientboundSetEquipmentPacket(7, List.of(Pair.of(EquipmentSlot.MAINHAND, stone))));
        fixture(output, "equipment_off_named", "equipment", ClientboundSetEquipmentPacket.STREAM_CODEC, new ClientboundSetEquipmentPacket(7, List.of(Pair.of(EquipmentSlot.OFFHAND, named))));
        fixture(output, "equipment_both_populated", "equipment", ClientboundSetEquipmentPacket.STREAM_CODEC, new ClientboundSetEquipmentPacket(300, List.of(Pair.of(EquipmentSlot.MAINHAND, stone), Pair.of(EquipmentSlot.OFFHAND, named))));
        return document;
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 2) throw new IllegalArgumentException("DATA FIXTURES [--export PATH] [--export-default-items] [--export-equipment]");
        Path data = Path.of(args[0]); Path fixtures = Path.of(args[1]); Path export = null; boolean defaults = false; boolean exportEquipment = false;
        for (int i = 2; i < args.length; i++) {
            if (args[i].equals("--export") && i + 1 < args.length) export = Path.of(args[++i]);
            else if (args[i].equals("--export-default-items")) defaults = true;
            else if (args[i].equals("--export-equipment")) exportEquipment = true;
            else throw new IllegalArgumentException("Unknown/incomplete argument " + args[i]);
        }
        JsonObject identity = json(data.resolve("prepared.json"));
        require(identity.get("version").getAsString().equals("26.3") && identity.get("protocol").getAsInt() == 777, "Wrong prepared data identity");
        packetIds(data);
        InventoryCodecOracle oracle = new InventoryCodecOracle(registries(data));
        int checked = oracle.check(json(fixtures));
        JsonObject generated = oracle.export(defaults);
        JsonObject equipment = oracle.equipmentExamples();
        int equipmentCount = oracle.check(equipment);
        if (exportEquipment) generated.getAsJsonArray("fixtures").addAll(equipment.getAsJsonArray("fixtures"));
        int generatedCount = oracle.check(generated);
        if (export != null) {
            Path parent = export.toAbsolutePath().getParent(); Files.createDirectories(parent);
            Files.writeString(export, new GsonBuilder().serializeNulls().setPrettyPrinting().disableHtmlEscaping().create().toJson(generated) + "\n", StandardCharsets.UTF_8);
            require(oracle.check(json(export)) == generatedCount, "Exported fixtures changed after serialization");
        }
        System.out.println("Minecraft 26.3 inventory oracle: " + checked + " input fixtures, " + generatedCount + " exported-domain fixtures and " + equipmentCount + " independent equipment cases passed");
    }
}
