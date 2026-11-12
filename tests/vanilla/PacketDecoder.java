import com.google.gson.JsonArray;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import com.google.gson.JsonPrimitive;
import com.mojang.serialization.JsonOps;
import com.mojang.serialization.Lifecycle;
import io.netty.buffer.Unpooled;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import net.minecraft.SharedConstants;
import net.minecraft.core.HolderLookup;
import net.minecraft.core.component.DataComponents;
import net.minecraft.core.component.DataComponentInitializers;
import net.minecraft.core.MappedRegistry;
import net.minecraft.core.RegistrationInfo;
import net.minecraft.core.Registry;
import net.minecraft.core.RegistryAccess;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.data.registries.VanillaRegistries;
import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.FriendlyByteBuf;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.ClientboundCustomPayloadPacket;
import net.minecraft.network.protocol.common.custom.BrandPayload;
import net.minecraft.network.protocol.game.ClientboundLevelChunkWithLightPacket;
import net.minecraft.network.protocol.game.ClientboundLoginPacket;
import net.minecraft.network.protocol.game.ClientboundSystemChatPacket;
import net.minecraft.network.protocol.game.ClientboundAddEntityPacket;
import net.minecraft.network.protocol.game.ClientboundEntityPositionSyncPacket;
import net.minecraft.network.protocol.game.ClientboundPlayerInfoUpdatePacket;
import net.minecraft.network.protocol.game.ClientboundPlayerAbilitiesPacket;
import net.minecraft.network.protocol.game.ClientboundSetEntityMotionPacket;
import net.minecraft.network.protocol.game.ClientboundTakeItemEntityPacket;
import net.minecraft.network.protocol.game.ClientboundSetEntityDataPacket;
import net.minecraft.network.protocol.game.ClientboundSetHealthPacket;
import net.minecraft.network.protocol.game.ClientboundRemoveEntitiesPacket;
import net.minecraft.network.protocol.game.ClientboundPlayerInfoRemovePacket;
import net.minecraft.network.protocol.game.ClientboundRotateHeadPacket;
import net.minecraft.world.entity.EntityTypes;
import net.minecraft.world.entity.Avatar;
import net.minecraft.world.entity.LivingEntity;
import net.minecraft.world.entity.PositionPath;
import net.minecraft.world.entity.item.ItemEntity;
import net.minecraft.world.item.ItemStack;
import net.minecraft.world.item.Items;
import net.minecraft.world.phys.Vec3;
import net.minecraft.network.protocol.game.ServerboundAttackPacket;
import net.minecraft.network.syncher.EntityDataAccessor;
import net.minecraft.network.syncher.EntityDataSerializers;
import net.minecraft.resources.Identifier;
import net.minecraft.resources.ResourceKey;
import net.minecraft.server.Bootstrap;
import net.minecraft.world.level.block.Blocks;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.level.block.state.properties.BlockStateProperties;
import net.minecraft.core.Direction;
import net.minecraft.world.level.chunk.LevelChunkSection;
import net.minecraft.world.level.chunk.PalettedContainerFactory;

public final class PacketDecoder {
    private static void require(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    private static Registry<?> registry(HolderLookup.Provider source, JsonObject definition) {
        ResourceKey key = ResourceKey.createRegistryKey(Identifier.parse(definition.get("name").getAsString()));
        HolderLookup.RegistryLookup lookup = source.lookupOrThrow(key);
        MappedRegistry result = new MappedRegistry(key, Lifecycle.stable());
        for (var entry : definition.getAsJsonArray("entries")) {
            var name = Identifier.parse(entry.getAsJsonObject().get("id").getAsString());
            ResourceKey entryKey = ResourceKey.create(key, name);
            var holder = (net.minecraft.core.Holder.Reference) lookup.getOrThrow(entryKey);
            result.register(entryKey, holder.value(), RegistrationInfo.BUILT_IN);
        }
        result.bindAllTagsToEmpty();
        return result.freeze();
    }

    private static RegistryAccess registries(Path data) throws Exception {
        SharedConstants.tryDetectVersion();
        Bootstrap.bootStrap();
        require(BlockState.CODEC.parse(JsonOps.INSTANCE,new JsonPrimitive("minecraft:grass_block")).getOrThrow() == Blocks.GRASS_BLOCK.defaultBlockState(), "Bare block names use default state");
        var partial=JsonParser.parseString("{\"id\":\"minecraft:oak_stairs\",\"properties\":{\"facing\":\"east\"}}");
        require(BlockState.CODEC.parse(JsonOps.INSTANCE,partial).getOrThrow() == Blocks.OAK_STAIRS.defaultBlockState().setValue(BlockStateProperties.HORIZONTAL_FACING,Direction.EAST), "Partial properties inherit default state");
        var source = VanillaRegistries.createWorldLookup();
        List<Registry<?>> registries = new ArrayList<>();
        BuiltInRegistries.REGISTRY.forEach(registries::add);
        JsonArray dynamic = JsonParser.parseString(Files.readString(data.resolve("registries-sync.json")))
            .getAsJsonObject().getAsJsonArray("registries");
        for (var entry : dynamic) registries.add(registry(source, entry.getAsJsonObject()));
        BuiltInRegistries.DATA_COMPONENT_INITIALIZERS.build(source).forEach(DataComponentInitializers.PendingComponents::apply);
        return new RegistryAccess.ImmutableRegistryAccess(registries).freeze();
    }

    private static String packetClass(String name, int id) {
        if (name.equals("brand_default") || name.equals("brand_unicode")) {
            require(id == 1, "Configuration custom payload packet ID");
            return "common.ClientboundCustomPayloadPacket";
        }
        if (name.equals("known_packs")) return "configuration.ClientboundSelectKnownPacks";
        if (name.startsWith("configuration_")) return switch (id) {
            case 3 -> "configuration.ClientboundFinishConfigurationPacket";
            case 7 -> "configuration.ClientboundRegistryDataPacket";
            case 13 -> "configuration.ClientboundUpdateEnabledFeaturesPacket";
            case 14 -> "common.ClientboundUpdateTagsPacket";
            default -> throw new AssertionError("Unexpected configuration ID " + id);
        };
        return "game." + switch (name) {
            case "login", "login_online", "login_survival", "login_survival_previous_creative", "login_creative_previous_survival" -> "ClientboundLoginPacket";
            case "position" -> "ClientboundPlayerPositionPacket";
            case "abilities", "abilities_creative_flying", "abilities_survival", "abilities_survival_requested_flight" -> "ClientboundPlayerAbilitiesPacket";
            case "chat" -> "ClientboundSystemChatPacket";
            case "chunk" -> "ClientboundLevelChunkWithLightPacket";
            case "center" -> "ClientboundSetChunkCacheCenterPacket";
            case "batch_start" -> "ClientboundChunkBatchStartPacket";
            case "batch_finish" -> "ClientboundChunkBatchFinishedPacket";
            case "spawn" -> "ClientboundSetDefaultSpawnPositionPacket";
            case "event" -> "ClientboundGameEventPacket";
            case "commands" -> "ClientboundCommandsPacket";
            case "player_info_add", "player_info_properties", "player_info_survival", "player_info_survival_properties" -> "ClientboundPlayerInfoUpdatePacket";
            case "player_info_remove" -> "ClientboundPlayerInfoRemovePacket";
            case "entity_player", "entity_pig", "entity_item_moving" -> "ClientboundAddEntityPacket";
            case "entity_position", "entity_position_checked" -> "ClientboundEntityPositionSyncPacket";
            case "entity_motion", "entity_motion_scaled" -> "ClientboundSetEntityMotionPacket";
            case "take_item_entity" -> "ClientboundTakeItemEntityPacket";
            case "entity_head" -> "ClientboundRotateHeadPacket";
            case "entity_remove" -> "ClientboundRemoveEntitiesPacket";
            case "entity_metadata", "mob_health", "item_metadata_stone", "item_metadata_named", "item_metadata_empty" -> "ClientboundSetEntityDataPacket";
            case "attack_request" -> "ServerboundAttackPacket";
            case "player_health" -> "ClientboundSetHealthPacket";
            case "boss_add", "boss_remove", "boss_update_0", "boss_update_1" -> "ClientboundBossEventPacket";
            default -> throw new AssertionError("Unknown fixture " + name);
        };
    }

    private static void checkChunk(ClientboundLevelChunkWithLightPacket packet, RegistryAccess registries) {
        require(packet.x() == -3 && packet.z() == 5, "Chunk coordinates");
        var buffer = packet.chunkData().getReadBuffer();
        var factory = PalettedContainerFactory.create(registries);
        for (int index = 0; index < 24; index++) {
            var section = new LevelChunkSection(factory);
            section.read(buffer);
            require(section.hasOnlyAir() == (index >= 8), "Non-air count in section " + index);
            require(!section.hasFluid(), "Unexpected fluid in section " + index);
            require(section.getBlockState(0, 0, 0).is(index == 0 ? Blocks.BEDROCK : index < 8 ? Blocks.STONE : Blocks.AIR), "First block in section " + index);
            require(section.getBlockState(15, 15, 15).is(index == 7 ? Blocks.GRASS_BLOCK : index < 8 ? Blocks.STONE : Blocks.AIR), "Last block in section " + index);
        }
        require(!buffer.isReadable(), "Trailing chunk section bytes");
    }

    private static Vec3 originalVelocity(Vec3 input) {
        var buffer = Unpooled.buffer();
        try {
            Vec3.LP_STREAM_CODEC.encode(buffer, input);
            Vec3 result = Vec3.LP_STREAM_CODEC.decode(buffer);
            require(!buffer.isReadable(), "Original velocity codec consumption");
            return result;
        } finally {
            buffer.release();
        }
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        Path data = Path.of(args[0]);
        Path fixtures = Path.of(args[1]);
        RegistryAccess registries = registries(data);
        var resolved=JsonParser.parseString(Files.readString(fixtures.resolve("block-resolver.json"))).getAsJsonArray();
        for (var value:resolved) {
            var entry=value.getAsJsonObject();
            var stateJson=new JsonObject();
            stateJson.add("id",entry.get("name"));
            stateJson.add("properties",entry.get("properties"));
            var state=BlockState.CODEC.parse(JsonOps.INSTANCE,entry.getAsJsonObject("properties").isEmpty()?entry.get("name"):stateJson).getOrThrow();
            require(Block.getId(state)==entry.get("state").getAsInt(),"Rust resolver differs from vanilla: "+entry);
        }
        var manifest = JsonParser.parseString(Files.readString(fixtures.resolve("manifest.json"))).getAsJsonArray();
        int checked = 0;
        for (var fixture : manifest) {
            String name = fixture.getAsString();
            byte[] fixtureBytes = Files.readAllBytes(fixtures.resolve(name + ".bin"));
            var buffer = new RegistryFriendlyByteBuf(Unpooled.wrappedBuffer(fixtureBytes), registries);
            try {
                int id = buffer.readVarInt();
                Class<?> type = Class.forName("net.minecraft.network.protocol." + packetClass(name, id));
                String codecField = name.startsWith("brand_") ? "CONFIG_STREAM_CODEC" : "STREAM_CODEC";
                var codec = (StreamCodec<RegistryFriendlyByteBuf, Object>) type.getField(codecField).get(null);
                Object packet = codec.decode(buffer);
                require(!buffer.isReadable(), name + " has trailing bytes");
                if (packet instanceof ClientboundCustomPayloadPacket custom) {
                    require(custom.payload() instanceof BrandPayload, "Original codec must recognize the brand payload");
                    BrandPayload brand = (BrandPayload) custom.payload();
                    String expected = name.equals("brand_default") ? "zerqon" : "teighto Привет 😀";
                    require(brand.type().id().toString().equals("minecraft:brand"), "Server brand channel");
                    require(brand.brand().equals(expected), "Server brand text and case preservation");
                    var encoded = new FriendlyByteBuf(Unpooled.buffer());
                    try {
                        encoded.writeVarInt(id);
                        ClientboundCustomPayloadPacket.CONFIG_STREAM_CODEC.encode(encoded, custom);
                        byte[] roundTrip = new byte[encoded.readableBytes()];
                        encoded.readBytes(roundTrip);
                        require(Arrays.equals(fixtureBytes, roundTrip), "Original brand codec byte-for-byte round trip");
                    } finally {
                        encoded.release();
                    }
                }
                if (packet instanceof ClientboundLevelChunkWithLightPacket chunk) checkChunk(chunk, registries);
                if (packet instanceof ClientboundLoginPacket login) {
                    require(id == 50, "Login packet ID");
                    require(login.playerId() == 7 && login.maxPlayers() == 20, "Login identity and capacity");
                    int expectedMode = name.startsWith("login_survival") ? 0 : 1;
                    int expectedPrevious = switch (name) {
                        case "login_survival_previous_creative" -> 1;
                        case "login_creative_previous_survival" -> 0;
                        default -> -1;
                    };
                    require(login.commonPlayerSpawnInfo().gameType().getId() == expectedMode, "Login game mode: " + name);
                    require(login.commonPlayerSpawnInfo().previousGameType().map(mode -> mode.getId()).orElse(-1) == expectedPrevious, "Login previous mode: " + name);
                    require(login.commonPlayerSpawnInfo().dimensionType().value().height() == 384, "Dimension registry ID");
                    require(login.onlineMode() == name.equals("login_online"), "Configured authentication mode: " + name);
                    require(!login.enforcesSecureChat(), "Secure chat enforcement is independent of authentication");
                }
                if (packet instanceof ClientboundPlayerAbilitiesPacket abilities) {
                    require(id == 65, "Abilities packet ID");
                    boolean creative = !name.startsWith("abilities_survival");
                    boolean flying = name.equals("abilities_creative_flying");
                    require(abilities.isInvulnerable() == creative && abilities.canFly() == creative && abilities.canInstabuild() == creative, "Server-owned ability flags: " + name);
                    require(abilities.isFlying() == flying, "Permitted flying state: " + name);
                    require(abilities.getFlyingSpeed() == 0.05f && abilities.getWalkingSpeed() == 0.1f, "Abilities speeds: " + name);
                }
                if (packet instanceof ClientboundSystemChatPacket chat) require(chat.content().getString().equals("Zerqon Привет 😀"), "NBT Unicode text");
                if (packet instanceof ClientboundPlayerInfoUpdatePacket info) {
                    require(id == 71, "TAB update packet ID");
                    var entry = info.entries().getFirst();
                    require(info.entries().size() == 1 && entry.profile().name().equals("TestPlayer"), "TAB profile");
                    int expectedMode = name.startsWith("player_info_survival") ? 0 : 1;
                    require(entry.listed() && entry.showHat() && entry.gameMode().getId() == expectedMode, "TAB flags: " + name);
                    if (name.endsWith("properties")) {
                        var texture=entry.profile().properties().get("textures").iterator().next();
                        require(texture.value().equals("texture-value") && texture.signature().equals("signed-value"), "Authenticated property preservation");
                    }
                }
                if (packet instanceof ClientboundLoginPacket || packet instanceof ClientboundPlayerAbilitiesPacket || packet instanceof ClientboundPlayerInfoUpdatePacket
                    || packet instanceof ClientboundAddEntityPacket || packet instanceof ClientboundSetEntityMotionPacket || packet instanceof ClientboundTakeItemEntityPacket
                    || packet instanceof ClientboundSetEntityDataPacket || packet instanceof ClientboundEntityPositionSyncPacket) {
                    var encoded = new RegistryFriendlyByteBuf(Unpooled.buffer(), registries);
                    try {
                        encoded.writeVarInt(id);
                        codec.encode(encoded, packet);
                        byte[] roundTrip = new byte[encoded.readableBytes()];
                        encoded.readBytes(roundTrip);
                        require(Arrays.equals(fixtureBytes, roundTrip), "Original packet round trip: " + name);
                    } finally {
                        encoded.release();
                    }
                }
                if (packet instanceof ClientboundPlayerInfoRemovePacket removed) require(removed.profileIds().size() == 1, "TAB removal");
                if (packet instanceof ClientboundAddEntityPacket entity) {
                    require(id == 1, "Entity spawn packet ID");
                    var expectedType = switch (name) {
                        case "entity_player" -> EntityTypes.PLAYER;
                        case "entity_item_moving" -> EntityTypes.ITEM;
                        default -> EntityTypes.PIG;
                    };
                    require(entity.getType() == expectedType, "Entity registry type");
                    require(entity.getX() == 1.0 && entity.getY() == 64.0 && entity.getZ() == -2.0, "Entity coordinates");
                    if (name.equals("entity_item_moving")) {
                        require(entity.getId() == 300 && entity.getUUID().equals(new java.util.UUID(0,42)), "Item entity identity");
                        require(entity.getMovement().equals(originalVelocity(new Vec3(0.1,0.2,-0.05))), "Item spawn LP velocity");
                    } else require(entity.getMovement().lengthSqr() == 0.0, "Zero low-precision velocity");
                    require(entity.getXRot() == -45.0f && entity.getYRot() == 90.0f, "Entity angles");
                }
                if (packet instanceof ClientboundEntityPositionSyncPacket sync) require(id == 35 && sync.id() == 300 && sync.position() instanceof PositionPath.Linear && sync.position().endPosition().equals(new Vec3(2.0,64.0,-3.0)) && sync.yRot() == 45.0f && sync.xRot() == -30.0f && sync.onGround(), "Linear entity position");
                if (packet instanceof ClientboundSetEntityMotionPacket motion) {
                    Vec3 expected = name.equals("entity_motion_scaled") ? new Vec3(512.25,-1024.5,4.75) : new Vec3(0.1,0.2,-0.05);
                    require(id == 103 && motion.id() == 300 && motion.movement().equals(originalVelocity(expected)), "Entity velocity packet");
                }
                if (packet instanceof ClientboundTakeItemEntityPacket taken) require(id == 127 && taken.getItemId() == 300 && taken.getPlayerId() == 301 && taken.getAmount() == 7, "Item collection packet");
                if (packet instanceof ClientboundRotateHeadPacket head) require(head.getYHeadRot() == 90.0f, "Head rotation");
                if (packet instanceof ClientboundRemoveEntitiesPacket remove) require(remove.entityIds().size() == 2 && remove.entityIds().getInt(1) == 301, "Entity removal");
                if (packet instanceof ClientboundSetEntityDataPacket entity) {
                    if (name.startsWith("item_metadata_")) {
                        var field = ItemEntity.class.getDeclaredField("DATA_ITEM");
                        field.setAccessible(true);
                        var accessor = (EntityDataAccessor<?>)field.get(null);
                        require(id == 101 && entity.id() == 300 && entity.packedItems().size() == 1, "Item metadata envelope");
                        var value = entity.packedItems().getFirst();
                        require(accessor.id() == 8 && EntityDataSerializers.getSerializedId(accessor.serializer()) == 7 && value.id() == accessor.id() && value.serializer() == EntityDataSerializers.ITEM_STACK, "Original item accessor and serializer");
                        var stack = (ItemStack)value.value();
                        if (name.equals("item_metadata_empty")) require(stack.isEmpty(), "Empty item metadata");
                        else {
                            boolean named = name.equals("item_metadata_named");
                            require(stack.is(Items.STONE) && stack.getCount() == (named ? 3 : 7), "Item metadata stack");
                            if (named) require(stack.get(DataComponents.CUSTOM_NAME).getString().equals("teighto") && stack.getComponentsPatch().size() == 1, "Item metadata component patch");
                            else require(stack.getComponentsPatch().isEmpty(), "Default item metadata has no patch");
                        }
                    } else {
                    var field = name.equals("mob_health")?LivingEntity.class.getDeclaredField("DATA_HEALTH_ID"):Avatar.class.getDeclaredField("DATA_PLAYER_MODE_CUSTOMISATION");
                    field.setAccessible(true);
                    var skin = (EntityDataAccessor<?>) field.get(null);
                    var value = entity.packedItems().getFirst();
                    if(name.equals("mob_health"))require(value.id()==skin.id()&&value.serializer()==EntityDataSerializers.FLOAT&&value.value().equals(6.0f),"Living entity health accessor");
                    else require(value.id() == skin.id() && value.serializer() == EntityDataSerializers.BYTE && value.value().equals((byte)127), "Vanilla skin-layer accessor");
                    }
                }
                if(packet instanceof ServerboundAttackPacket attack)require(attack.entityId()==300,"Serverbound attack ID");
                if (packet instanceof ClientboundSetHealthPacket health) require(health.getHealth() == 20.0f && health.getFood() == 20 && health.getSaturation() == 5.0f, "Health and hunger");
                checked++;
            } catch (Throwable failure) {
                throw new AssertionError("Vanilla failed to decode " + name, failure);
            } finally {
                buffer.release();
            }
        }
        System.out.println("Vanilla 26.3 decoded " + checked + " Zerqon packets; 24 chunk sections and Unicode verified.");
    }
}
