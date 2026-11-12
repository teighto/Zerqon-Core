import net.minecraft.SharedConstants;
import net.minecraft.server.Bootstrap;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.core.BlockPos;
import net.minecraft.world.level.EmptyBlockGetter;
import com.google.gson.*;
public class MiningFacts {
 public static void main(String[] args) throws Exception {
  SharedConstants.tryDetectVersion(); Bootstrap.bootStrap(); JsonObject result=new JsonObject();
  for(var block:BuiltInRegistries.BLOCK) {var state=block.defaultBlockState(); JsonObject value=new JsonObject(); value.addProperty("hardness",state.getDestroySpeed(EmptyBlockGetter.INSTANCE,BlockPos.ZERO)); value.addProperty("requires_tool",state.requiresCorrectToolForDrops()); result.add(BuiltInRegistries.BLOCK.getKey(block).toString(),value);}
  java.nio.file.Files.writeString(java.nio.file.Path.of(args[0]),new Gson().toJson(result));
 }
}
