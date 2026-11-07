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
import net.minecraft.network.protocol.game.ClientboundSetEntityDataPacket;
import net.minecraft.network.protocol.game.ClientboundSetHealthPacket;
import net.minecraft.network.protocol.game.ClientboundRemoveEntitiesPacket;
import net.minecraft.network.protocol.game.ClientboundPlayerInfoRemovePacket;
import net.minecraft.network.protocol.game.ClientboundRotateHeadPacket;
import net.minecraft.world.entity.EntityTypes;
import net.minecraft.world.entity.Avatar;
import net.minecraft.world.entity.LivingEntity;
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
            case "login" -> "ClientboundLoginPacket";
            case "position" -> "ClientboundPlayerPositionPacket";
            case "abilities" -> "ClientboundPlayerAbilitiesPacket";
            case "chat" -> "ClientboundSystemChatPacket";
            case "chunk" -> "ClientboundLevelChunkWithLightPacket";
            case "center" -> "ClientboundSetChunkCacheCenterPacket";
            case "batch_start" -> "ClientboundChunkBatchStartPacket";
            case "batch_finish" -> "ClientboundChunkBatchFinishedPacket";
            case "spawn" -> "ClientboundSetDefaultSpawnPositionPacket";
            case "event" -> "ClientboundGameEventPacket";
            case "commands" -> "ClientboundCommandsPacket";
            case "player_info_add", "player_info_properties" -> "ClientboundPlayerInfoUpdatePacket";
            case "player_info_remove" -> "ClientboundPlayerInfoRemovePacket";
            case "entity_player", "entity_pig" -> "ClientboundAddEntityPacket";
            case "entity_position" -> "ClientboundEntityPositionSyncPacket";
            case "entity_head" -> "ClientboundRotateHeadPacket";
            case "entity_remove" -> "ClientboundRemoveEntitiesPacket";
            case "entity_metadata", "mob_health" -> "ClientboundSetEntityDataPacket";
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
                var codec = (StreamCodec<RegistryFriendlyByteBuf, ?>) type.getField(codecField).get(null);
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
                    require(login.playerId() == 7 && login.maxPlayers() == 20, "Login identity and capacity");
                    require(login.commonPlayerSpawnInfo().gameType().getId() == 1, "Creative mode");
                    require(login.commonPlayerSpawnInfo().dimensionType().value().height() == 384, "Dimension registry ID");
                }
                if (packet instanceof ClientboundSystemChatPacket chat) require(chat.content().getString().equals("Zerqon Привет 😀"), "NBT Unicode text");
                if (packet instanceof ClientboundPlayerInfoUpdatePacket info) {
                    var entry = info.entries().getFirst();
                    require(info.entries().size() == 1 && entry.profile().name().equals("TestPlayer"), "TAB profile");
                    require(entry.listed() && entry.showHat() && entry.gameMode().getId() == 1, "TAB flags");
                    if (name.equals("player_info_properties")) {
                        var texture=entry.profile().properties().get("textures").iterator().next();
                        require(texture.value().equals("texture-value") && texture.signature().equals("signed-value"), "Authenticated property preservation");
                    }
                }
                if (packet instanceof ClientboundPlayerInfoRemovePacket removed) require(removed.profileIds().size() == 1, "TAB removal");
                if (packet instanceof ClientboundAddEntityPacket entity) {
                    require(entity.getType() == (name.equals("entity_player") ? EntityTypes.PLAYER : EntityTypes.PIG), "Entity registry type");
                    require(entity.getX() == 1.0 && entity.getY() == 64.0 && entity.getZ() == -2.0, "Entity coordinates");
                    require(entity.getMovement().lengthSqr() == 0.0, "Zero low-precision velocity");
                    require(entity.getXRot() == -45.0f && entity.getYRot() == 90.0f, "Entity angles");
                }
                if (packet instanceof ClientboundEntityPositionSyncPacket sync) require(sync.id() == 300 && sync.position().endPosition().x == 2.0 && sync.onGround(), "Linear entity position");
                if (packet instanceof ClientboundRotateHeadPacket head) require(head.getYHeadRot() == 90.0f, "Head rotation");
                if (packet instanceof ClientboundRemoveEntitiesPacket remove) require(remove.entityIds().size() == 2 && remove.entityIds().getInt(1) == 301, "Entity removal");
                if (packet instanceof ClientboundSetEntityDataPacket entity) {
                    var field = name.equals("mob_health")?LivingEntity.class.getDeclaredField("DATA_HEALTH_ID"):Avatar.class.getDeclaredField("DATA_PLAYER_MODE_CUSTOMISATION");
                    field.setAccessible(true);
                    var skin = (EntityDataAccessor<?>) field.get(null);
                    var value = entity.packedItems().getFirst();
                    if(name.equals("mob_health"))require(value.id()==skin.id()&&value.serializer()==EntityDataSerializers.FLOAT&&value.value().equals(6.0f),"Living entity health accessor");
                    else require(value.id() == skin.id() && value.serializer() == EntityDataSerializers.BYTE && value.value().equals((byte)127), "Vanilla skin-layer accessor");
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
