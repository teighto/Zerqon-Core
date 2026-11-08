import com.google.gson.GsonBuilder;
import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import io.netty.buffer.Unpooled;
import it.unimi.dsi.fastutil.ints.Int2ObjectOpenHashMap;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Instant;
import java.util.Arrays;
import java.util.BitSet;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import java.util.Set;
import net.minecraft.commands.arguments.ArgumentSignatures;
import net.minecraft.core.BlockPos;
import net.minecraft.core.Direction;
import net.minecraft.core.RegistryAccess;
import net.minecraft.core.component.DataComponents;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.network.HashedPatchMap;
import net.minecraft.network.HashedStack;
import net.minecraft.network.RegistryFriendlyByteBuf;
import net.minecraft.network.chat.LastSeenMessages;
import net.minecraft.network.chat.MessageSignature;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.ServerboundKeepAlivePacket;
import net.minecraft.network.protocol.game.ServerboundAcceptTeleportationPacket;
import net.minecraft.network.protocol.game.ServerboundAttackPacket;
import net.minecraft.network.protocol.game.ServerboundChatCommandPacket;
import net.minecraft.network.protocol.game.ServerboundChatCommandSignedPacket;
import net.minecraft.network.protocol.game.ServerboundContainerClickPacket;
import net.minecraft.network.protocol.game.ServerboundMovePlayerPacket;
import net.minecraft.network.protocol.game.ServerboundPlayerAbilitiesPacket;
import net.minecraft.network.protocol.game.ServerboundPlayerActionPacket;
import net.minecraft.network.protocol.game.ServerboundSetCarriedItemPacket;
import net.minecraft.network.protocol.game.ServerboundSetCreativeModeSlotPacket;
import net.minecraft.network.protocol.game.ServerboundUseItemOnPacket;
import net.minecraft.world.InteractionHand;
import net.minecraft.world.inventory.ContainerInput;
import net.minecraft.world.item.ItemStack;
import net.minecraft.world.item.Items;
import net.minecraft.world.phys.BlockHitResult;
import net.minecraft.world.phys.Vec3;

public final class ClientPacketOracle {
    private final RegistryAccess registries;
    private final Object inventory;
    private final Method inventoryFields;
    private static final Map<String,Integer> IDS = Map.ofEntries(
        Map.entry("accept_teleportation",0),Map.entry("attack",1),Map.entry("chat_command",7),Map.entry("chat_command_signed",8),
        Map.entry("container_click",18),Map.entry("keep_alive",28),Map.entry("move_player_pos",30),Map.entry("move_player_pos_rot",31),
        Map.entry("move_player_rot",32),Map.entry("move_player_status_only",33),Map.entry("player_abilities",40),Map.entry("player_action",41),Map.entry("set_carried_item",54),
        Map.entry("set_creative_mode_slot",57),Map.entry("use_item_on",66));

    private ClientPacketOracle(Path data) throws Exception {
        var initializer=InventoryCodecOracle.class.getDeclaredMethod("registries",Path.class);
        initializer.setAccessible(true);registries=(RegistryAccess)initializer.invoke(null,data);
        var constructor=InventoryCodecOracle.class.getDeclaredConstructor(RegistryAccess.class);
        constructor.setAccessible(true);inventory=constructor.newInstance(registries);
        inventoryFields=InventoryCodecOracle.class.getDeclaredMethod("normalized",Object.class);inventoryFields.setAccessible(true);
        var packets=read(data.resolve("generated/reports/packets.json")).getAsJsonObject("play").getAsJsonObject("serverbound");
        for(var entry:IDS.entrySet())require(packets.getAsJsonObject("minecraft:"+entry.getKey()).get("protocol_id").getAsInt()==entry.getValue(),"Wrong packet ID for "+entry.getKey());
    }

    private static void require(boolean condition,String message){if(!condition)throw new AssertionError(message);}
    private static JsonObject read(Path path)throws Exception{
        require(Files.size(path)<=4*1024*1024,"Fixture JSON exceeds4MiB");
        return JsonParser.parseString(Files.readString(path,StandardCharsets.UTF_8)).getAsJsonObject();
    }
    private RegistryFriendlyByteBuf buffer(byte[]bytes){return new RegistryFriendlyByteBuf(Unpooled.wrappedBuffer(bytes),registries);}
    private Object decode(String kind,byte[]bytes){
        var input=buffer(bytes);
        try{
            Object packet=switch(kind){
                case "accept_teleportation" -> ServerboundAcceptTeleportationPacket.STREAM_CODEC.decode(input);
                case "attack" -> ServerboundAttackPacket.STREAM_CODEC.decode(input);
                case "chat_command" -> ServerboundChatCommandPacket.STREAM_CODEC.decode(input);
                case "chat_command_signed" -> ServerboundChatCommandSignedPacket.STREAM_CODEC.decode(input);
                case "container_click" -> ServerboundContainerClickPacket.STREAM_CODEC.decode(input);
                case "keep_alive" -> ServerboundKeepAlivePacket.STREAM_CODEC.decode(input);
                case "move_player_pos" -> ServerboundMovePlayerPacket.Pos.STREAM_CODEC.decode(input);
                case "move_player_pos_rot" -> ServerboundMovePlayerPacket.PosRot.STREAM_CODEC.decode(input);
                case "move_player_rot" -> ServerboundMovePlayerPacket.Rot.STREAM_CODEC.decode(input);
                case "move_player_status_only" -> ServerboundMovePlayerPacket.StatusOnly.STREAM_CODEC.decode(input);
                case "player_abilities" -> ServerboundPlayerAbilitiesPacket.STREAM_CODEC.decode(input);
                case "player_action" -> ServerboundPlayerActionPacket.STREAM_CODEC.decode(input);
                case "set_carried_item" -> ServerboundSetCarriedItemPacket.STREAM_CODEC.decode(input);
                case "set_creative_mode_slot" -> ServerboundSetCreativeModeSlotPacket.STREAM_CODEC.decode(input);
                case "use_item_on" -> ServerboundUseItemOnPacket.STREAM_CODEC.decode(input);
                default -> throw new IllegalArgumentException("Unknown kind: "+kind);
            };
            if(input.isReadable())throw new IllegalArgumentException("Trailing packet bytes: "+input.readableBytes());
            return packet;
        }finally{input.release();}
    }

    private JsonElement fields(Object value)throws Exception{
        JsonObject result=new JsonObject();
        if(value instanceof ServerboundAcceptTeleportationPacket packet){
            result.addProperty("id",packet.id());result.add("position",numbers(packet.x(),packet.y(),packet.z()));result.add("rotation",numbers(packet.yRot(),packet.xRot()));
        }else if(value instanceof ServerboundAttackPacket packet){result.addProperty("entity_id",packet.entityId());
        }else if(value instanceof ServerboundKeepAlivePacket packet){result.addProperty("nonce",packet.getId());
        }else if(value instanceof ServerboundPlayerAbilitiesPacket packet){result.addProperty("flying",packet.isFlying());
        }else if(value instanceof ServerboundChatCommandPacket packet){result.addProperty("command",packet.command());
        }else if(value instanceof ServerboundChatCommandSignedPacket packet){
            result.addProperty("command",packet.command());result.addProperty("timestamp_ms",packet.timeStamp().toEpochMilli());result.addProperty("salt",packet.salt());
            JsonArray signatures=new JsonArray();for(var entry:packet.argumentSignatures().entries()){JsonObject signature=new JsonObject();signature.addProperty("name",entry.name());signature.addProperty("bytes",HexFormat.of().formatHex(entry.signature().bytes()));signatures.add(signature);}result.add("signatures",signatures);
            result.addProperty("last_seen_offset",packet.lastSeenMessages().offset());JsonArray bits=new JsonArray();packet.lastSeenMessages().acknowledged().stream().forEach(bits::add);result.add("last_seen_bits",bits);result.addProperty("last_seen_checksum",packet.lastSeenMessages().checksum());
        }else if(value instanceof ServerboundMovePlayerPacket packet){
            result.addProperty("has_position",packet.hasPosition());result.addProperty("has_rotation",packet.hasRotation());
            if(packet.hasPosition())result.add("position",numbers(packet.getX(0),packet.getY(0),packet.getZ(0)));
            if(packet.hasRotation())result.add("rotation",numbers(packet.getYRot(0),packet.getXRot(0)));
            result.addProperty("on_ground",packet.isOnGround());result.addProperty("horizontal_collision",packet.horizontalCollision());
        }else if(value instanceof ServerboundPlayerActionPacket packet){
            result.addProperty("action",packet.getAction().ordinal());result.add("block",numbers(packet.getPos().getX(),packet.getPos().getY(),packet.getPos().getZ()));result.addProperty("face",packet.getDirection().get3DDataValue());result.addProperty("sequence",packet.getSequence());
        }else{return (JsonElement)inventoryFields.invoke(inventory,value);}
        return result;
    }
    private static JsonArray numbers(double...values){JsonArray result=new JsonArray();for(double value:values)result.add(value);return result;}
    private <T> void fixture(JsonArray cases,String name,String kind,StreamCodec<? super RegistryFriendlyByteBuf,T> codec,T value)throws Exception{
        var output=new RegistryFriendlyByteBuf(Unpooled.buffer(),registries);
        byte[]body;
        try{codec.encode(output,value);body=new byte[output.readableBytes()];output.readBytes(body);}finally{output.release();}
        fixtureBody(cases,name,kind,body,fields(value));
    }
    private void fixtureBody(JsonArray cases,String name,String kind,byte[]body,JsonElement expected)throws Exception{
        require(expected.equals(fields(decode(kind,body))),"Original decode changed fields: "+name);
        JsonObject fixture=new JsonObject();fixture.addProperty("name",name);fixture.addProperty("kind",kind);fixture.addProperty("packet_id",IDS.get(kind));fixture.addProperty("body",HexFormat.of().formatHex(body));fixture.add("fields",expected);
        var packet=new RegistryFriendlyByteBuf(Unpooled.buffer(),registries);
        try{packet.writeVarInt(IDS.get(kind));packet.writeBytes(body);byte[]payload=new byte[packet.readableBytes()];packet.readBytes(payload);fixture.addProperty("packet",HexFormat.of().formatHex(payload));packet.clear();packet.writeVarInt(payload.length);packet.writeBytes(payload);byte[]frame=new byte[packet.readableBytes()];packet.readBytes(frame);fixture.addProperty("frame",HexFormat.of().formatHex(frame));}finally{packet.release();}
        cases.add(fixture);
    }

    private JsonObject export()throws Exception{
        JsonArray cases=new JsonArray();
        fixture(cases,"teleport_initial","accept_teleportation",ServerboundAcceptTeleportationPacket.STREAM_CODEC,new ServerboundAcceptTeleportationPacket(1,.5,64,.5,0,0));
        fixture(cases,"teleport_negative","accept_teleportation",ServerboundAcceptTeleportationPacket.STREAM_CODEC,new ServerboundAcceptTeleportationPacket(129,-17.25,-60,33.5,180,-45.5f));
        fixture(cases,"teleport_max_id","accept_teleportation",ServerboundAcceptTeleportationPacket.STREAM_CODEC,new ServerboundAcceptTeleportationPacket(Integer.MAX_VALUE,30_000_000,-20_000_000,-30_000_000,720.25f,89.5f));
        fixture(cases,"attack_multibyte","attack",ServerboundAttackPacket.STREAM_CODEC,new ServerboundAttackPacket(300));
        fixture(cases,"keepalive_negative","keep_alive",ServerboundKeepAlivePacket.STREAM_CODEC,new ServerboundKeepAlivePacket(-0x102030405060708L));
        fixture(cases,"command_unicode","chat_command",ServerboundChatCommandPacket.STREAM_CODEC,new ServerboundChatCommandPacket("say Привет teighto"));
        fixture(cases,"command_length_300","chat_command",ServerboundChatCommandPacket.STREAM_CODEC,new ServerboundChatCommandPacket("say "+"a".repeat(296)));
        fixture(cases,"signed_empty","chat_command_signed",ServerboundChatCommandSignedPacket.STREAM_CODEC,new ServerboundChatCommandSignedPacket("version",Instant.ofEpochMilli(1794038400123L),0,ArgumentSignatures.EMPTY,new LastSeenMessages.Update(0,new BitSet(),(byte)0)));
        byte[]signature=new byte[256];for(int i=0;i<signature.length;i++)signature[i]=(byte)i;
        BitSet bits=new BitSet();for(int index:new int[]{0,8,19})bits.set(index);
        fixture(cases,"signed_arguments","chat_command_signed",ServerboundChatCommandSignedPacket.STREAM_CODEC,new ServerboundChatCommandSignedPacket("say teighto",Instant.ofEpochMilli(1794038400123L),-9,new ArgumentSignatures(List.of(new ArgumentSignatures.Entry("message",new MessageSignature(signature)))),new LastSeenMessages.Update(130,bits,(byte)-37)));
        for(int flags=0;flags<4;flags++){
            boolean ground=(flags&1)!=0,collision=(flags&2)!=0;
            fixture(cases,"move_pos_flags"+flags,"move_player_pos",ServerboundMovePlayerPacket.Pos.STREAM_CODEC,new ServerboundMovePlayerPacket.Pos(-17.25,-60,33.5,ground,collision));
            fixture(cases,"move_posrot_flags"+flags,"move_player_pos_rot",ServerboundMovePlayerPacket.PosRot.STREAM_CODEC,new ServerboundMovePlayerPacket.PosRot(-17.25,-60,33.5,180,-45.5f,ground,collision));
            fixture(cases,"move_rot_flags"+flags,"move_player_rot",ServerboundMovePlayerPacket.Rot.STREAM_CODEC,new ServerboundMovePlayerPacket.Rot(720.25f,89.5f,ground,collision));
            fixture(cases,"move_status_flags"+flags,"move_player_status_only",ServerboundMovePlayerPacket.StatusOnly.STREAM_CODEC,new ServerboundMovePlayerPacket.StatusOnly(ground,collision));
        }
        for(int flags:new int[]{0,2,13,15,255}){
            byte[]body={(byte)flags};
            fixtureBody(cases,"abilities_flags"+flags,"player_abilities",body,fields(decode("player_abilities",body)));
        }
        for(var action:ServerboundPlayerActionPacket.Action.values())fixture(cases,"action_"+action.ordinal(),"player_action",ServerboundPlayerActionPacket.STREAM_CODEC,new ServerboundPlayerActionPacket(action,new BlockPos(-17,-60,33),Direction.values()[action.ordinal()%6],130));
        fixture(cases,"carried_last","set_carried_item",ServerboundSetCarriedItemPacket.STREAM_CODEC,new ServerboundSetCarriedItemPacket(8));
        fixture(cases,"creative_empty","set_creative_mode_slot",ServerboundSetCreativeModeSlotPacket.STREAM_CODEC,new ServerboundSetCreativeModeSlotPacket(36,ItemStack.EMPTY));
        fixture(cases,"creative_stone64","set_creative_mode_slot",ServerboundSetCreativeModeSlotPacket.STREAM_CODEC,new ServerboundSetCreativeModeSlotPacket(44,new ItemStack(Items.STONE,64)));
        var changes=new Int2ObjectOpenHashMap<HashedStack>();changes.put(36,new HashedStack.ActualItem(BuiltInRegistries.ITEM.wrapAsHolder(Items.STONE),64,new HashedPatchMap(Map.of(DataComponents.MAX_STACK_SIZE,0x12345678),Set.of(DataComponents.CUSTOM_NAME))));
        for(var mode:ContainerInput.values())fixture(cases,"click_"+mode.ordinal(),"container_click",ServerboundContainerClickPacket.STREAM_CODEC,new ServerboundContainerClickPacket(0,32767,(short)36,(byte)0,mode,changes,HashedStack.EMPTY));
        for(var hand:InteractionHand.values())for(var face:Direction.values())for(int flags=0;flags<4;flags++)fixture(cases,"use_"+hand+"_"+face+"_"+flags,"use_item_on",ServerboundUseItemOnPacket.STREAM_CODEC,new ServerboundUseItemOnPacket(hand,new BlockHitResult(new Vec3(-16.75,-59.5,33.75),face,new BlockPos(-17,-60,33),(flags&1)!=0,(flags&2)!=0),130));
        JsonObject document=new JsonObject();document.addProperty("minecraft","26.3");document.addProperty("protocol",777);document.add("cases",cases);return document;
    }

    private void rejected(String kind,byte[]body){
        try{decode(kind,body);}catch(RuntimeException expected){return;}
        throw new AssertionError("Malformed "+kind+" fixture unexpectedly decoded");
    }
    private int verify(JsonObject document)throws Exception{
        require(document.get("minecraft").getAsString().equals("26.3")&&document.get("protocol").getAsInt()==777,"Wrong fixture version");
        JsonArray cases=document.getAsJsonArray("cases");require(cases.size()<=10000,"Too many cases");
        for(var element:cases){var entry=element.getAsJsonObject();String kind=entry.get("kind").getAsString(),name=entry.get("name").getAsString();require(IDS.get(kind)==entry.get("packet_id").getAsInt(),"Packet ID "+name);
            String hex=entry.get("body").getAsString();require(hex.length()<=262144,"Packet too large");byte[]body=HexFormat.of().parseHex(hex);require(entry.get("fields").equals(fields(decode(kind,body))),"Decoded fields differ: "+name);
            var packet=buffer(HexFormat.of().parseHex(entry.get("packet").getAsString()));try{require(packet.readVarInt()==IDS.get(kind),"Packet ID bytes "+name);byte[]actual=new byte[packet.readableBytes()];packet.readBytes(actual);require(Arrays.equals(actual,body),"Packet body "+name);}finally{packet.release();}
            var frame=buffer(HexFormat.of().parseHex(entry.get("frame").getAsString()));try{int length=frame.readVarInt();require(length==frame.readableBytes(),"Frame length "+name);byte[]actual=new byte[length];frame.readBytes(actual);require(HexFormat.of().formatHex(actual).equals(entry.get("packet").getAsString()),"Frame payload "+name);}finally{frame.release();}
            rejected(kind,Arrays.copyOf(body,body.length+1));rejected(kind,Arrays.copyOf(body,body.length-1));
        }
        return cases.size();
    }
    public static void main(String[]args)throws Exception{
        var oracle=new ClientPacketOracle(Path.of(args[0]));Path fixtures=Path.of(args[1]);
        if(args.length==4&&args[2].equals("--export")){Path output=Path.of(args[3]);Files.createDirectories(output.toAbsolutePath().getParent());Files.writeString(output,new GsonBuilder().setPrettyPrinting().serializeNulls().disableHtmlEscaping().create().toJson(oracle.export())+"\n",StandardCharsets.UTF_8);}
        int count=oracle.verify(read(fixtures));System.out.println("Original Minecraft26.3 codecs verified "+count+" client packets, all fields, frames, truncation and trailing-byte rejection");
    }
}
