import com.google.gson.GsonBuilder;
import com.google.gson.JsonArray;
import com.google.gson.JsonObject;
import java.lang.reflect.Method;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Random;
import net.minecraft.core.BlockPos;
import net.minecraft.core.Direction;
import net.minecraft.core.RegistryAccess;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.network.syncher.SynchedEntityData;
import net.minecraft.world.damagesource.DamageSource;
import net.minecraft.world.entity.Entity;
import net.minecraft.world.entity.EntityType;
import net.minecraft.world.entity.EntityTypes;
import net.minecraft.world.entity.ai.attributes.Attributes;
import net.minecraft.world.entity.animal.pig.Pig;
import net.minecraft.world.entity.animal.cow.AbstractCow;
import net.minecraft.world.entity.monster.zombie.Zombie;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.Blocks;
import net.minecraft.world.level.block.SlabBlock;
import net.minecraft.world.level.block.StairBlock;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.level.block.state.properties.SlabType;
import net.minecraft.world.level.EmptyBlockGetter;
import net.minecraft.world.level.storage.ValueInput;
import net.minecraft.world.level.storage.ValueOutput;
import net.minecraft.world.phys.AABB;
import net.minecraft.world.phys.Vec3;
import net.minecraft.world.phys.shapes.CollisionContext;
import net.minecraft.world.phys.shapes.Shapes;
import net.minecraft.world.phys.shapes.VoxelShape;

public final class EntityPhysicsOracle {
    private record Cell(BlockPos position,BlockState state) {}
    private record Scenario(String name,boolean floor,Vec3 position,Vec3 movement,boolean grounded,List<Cell> cells) {}
    private static final class Probe extends Entity {
        private final float step;
        Probe(EntityType<?> type,FixtureLevel level,float step){super(type,level);this.step=step;}
        @Override public float maxUpStep(){return step;}
        @Override protected void defineSynchedData(SynchedEntityData.Builder builder) {}
        @Override public boolean hurtServer(ServerLevel level,DamageSource source,float amount){throw new AssertionError("Unexpected damage");}
        @Override protected void readAdditionalSaveData(ValueInput input){throw new AssertionError("Unexpected save read");}
        @Override protected void addAdditionalSaveData(ValueOutput output){throw new AssertionError("Unexpected save write");}
    }
    private static JsonArray numbers(double...values){JsonArray result=new JsonArray();for(double value:values)result.add(value);return result;}
    private static Cell at(int x,int y,int z,BlockState state){return new Cell(new BlockPos(x,y,z),state);}
    private static List<Scenario> scenarios(EntityType<?> type){
        var stone=Blocks.STONE.defaultBlockState();var lower=Blocks.OAK_SLAB.defaultBlockState();var upper=lower.setValue(SlabBlock.TYPE,SlabType.TOP);var stair=Blocks.OAK_STAIRS.defaultBlockState().setValue(StairBlock.FACING,Direction.EAST);
        var out=new ArrayList<Scenario>();
        out.add(new Scenario("fall_bottom",true,new Vec3(.5,4,.5),new Vec3(0,-8,0),false,List.of(at(0,0,0,lower))));
        out.add(new Scenario("fall_top",true,new Vec3(.5,4,.5),new Vec3(0,-8,0),false,List.of(at(0,0,0,upper))));
        out.add(new Scenario("fall_stair_low",true,new Vec3(0,4,.5),new Vec3(0,-8,0),false,List.of(at(0,0,0,stair))));
        out.add(new Scenario("fall_stair_high",true,new Vec3(.75,4,.5),new Vec3(0,-8,0),false,List.of(at(0,0,0,stair))));
        out.add(new Scenario("step_bottom",true,new Vec3(.5,0,.5),new Vec3(.75,-.08,0),true,List.of(at(1,0,0,lower))));
        out.add(new Scenario("step_top_denied",true,new Vec3(.5,0,.5),new Vec3(.75,-.08,0),true,List.of(at(1,0,0,upper))));
        out.add(new Scenario("stair_first",true,new Vec3(.5,0,.5),new Vec3(.4,-.08,0),true,List.of(at(1,0,0,stair))));
        out.add(new Scenario("stair_second",true,new Vec3(.9,.5,.5),new Vec3(.4,-.08,0),true,List.of(at(1,0,0,stair))));
        var ceiling=type==EntityTypes.COW?upper:stone;int ceilingY=type==EntityTypes.ZOMBIE?2:1;
        out.add(new Scenario("headroom",true,new Vec3(.5,0,.5),new Vec3(.75,-.08,.2),true,List.of(at(1,0,0,lower),at(0,ceilingY,0,ceiling),at(1,ceilingY,0,ceiling))));
        out.add(new Scenario("negative_stair",true,new Vec3(-2.5,0,-2.5),new Vec3(.4,-.08,0),true,List.of(at(-2,0,-3,stair))));
        out.add(new Scenario("diagonal_x_first",true,new Vec3(.5,0,.5),new Vec3(3,-.08,2),true,List.of(at(1,0,1,stone),at(1,1,1,stone),at(1,2,1,stone))));
        out.add(new Scenario("diagonal_z_first",true,new Vec3(.5,0,.5),new Vec3(2,-.08,3),true,List.of(at(1,0,1,stone),at(1,1,1,stone),at(1,2,1,stone))));
        out.add(new Scenario("negative_wall",true,new Vec3(.5,0,.5),new Vec3(-5,-.08,0),true,List.of(at(-2,0,0,stone),at(-2,1,0,stone),at(-2,2,0,stone))));
        out.add(new Scenario("edge_fall",false,new Vec3(1.6,0,.5),new Vec3(0,-.08,0),true,List.of(at(0,-1,0,stone))));
        out.add(new Scenario("epsilon_free_horizontal",true,new Vec3(.5,0,.5),new Vec3(1e-8,0,0),true,List.of()));
        out.add(new Scenario("epsilon_while_falling",true,new Vec3(.5,0,.5),new Vec3(1e-8,-.08,0),true,List.of()));
        double nearWall=1.0-type.getDimensions().width()/2.0-5e-8;
        out.add(new Scenario("epsilon_multiple_blocks",true,new Vec3(nearWall,0,.5),new Vec3(.1,0,0),false,List.of(at(1,0,0,stone),at(1,1,0,stone),at(1,2,0,stone))));
        out.add(new Scenario("epsilon_single_stair",false,new Vec3(nearWall,0,.5),new Vec3(.1,0,0),false,List.of(at(1,0,0,stair))));
        var random=new Random(0x263777);
        BlockState[] choices={stone,lower,upper,stair,stair.setValue(StairBlock.FACING,Direction.WEST),stair.setValue(StairBlock.FACING,Direction.NORTH),stair.setValue(StairBlock.FACING,Direction.SOUTH)};
        for(int scenario=0;scenario<128;scenario++){
            var cells=new ArrayList<Cell>();
            for(int x=2;x<6;x++)for(int z=-3;z<4;z++)if(random.nextInt(3)==0){int height=random.nextInt(3);for(int y=0;y<=height;y++)cells.add(at(x,y,z,choices[random.nextInt(choices.length)]));}
            out.add(new Scenario("seeded_"+scenario,true,new Vec3(.5,0,.5),new Vec3(random.nextDouble()*5,-random.nextDouble()*.3,random.nextDouble()*6-3),true,cells));
        }
        return out;
    }
    public static void main(String[]args)throws Exception{
        Method initialize=InventoryCodecOracle.class.getDeclaredMethod("registries",Path.class);initialize.setAccessible(true);var registries=(RegistryAccess)initialize.invoke(null,Path.of(args[0]));
        Method collide=Entity.class.getDeclaredMethod("collide",Vec3.class);collide.setAccessible(true);
        var onGround=Entity.class.getDeclaredField("onGround");onGround.setAccessible(true);
        Method clip=Entity.class.getDeclaredMethod("collideWithShapes",Vec3.class,AABB.class,List.class);clip.setAccessible(true);
        JsonArray cases=new JsonArray();
        EntityType<?>[]types={EntityTypes.PIG,EntityTypes.COW,EntityTypes.ZOMBIE};
        double[]steps={Pig.createAttributes().build().getBaseValue(Attributes.STEP_HEIGHT),AbstractCow.createAttributes().build().getBaseValue(Attributes.STEP_HEIGHT),Zombie.createAttributes().build().getBaseValue(Attributes.STEP_HEIGHT)};
        for(int index=0;index<types.length;index++)for(var scenario:scenarios(types[index])){
            var shapes=new ArrayList<VoxelShape>();if(scenario.floor)shapes.add(Shapes.box(-64,-32,-64,64,0,64));
            JsonArray cells=new JsonArray();
            for(var cell:scenario.cells){shapes.add(cell.state.getCollisionShape(EmptyBlockGetter.INSTANCE,cell.position,CollisionContext.empty()).move(cell.position.getX(),cell.position.getY(),cell.position.getZ()));JsonArray record=new JsonArray();record.add(cell.position.getX());record.add(cell.position.getY());record.add(cell.position.getZ());record.add(Block.getId(cell.state));cells.add(record);}
            var level=new FixtureLevel(registries,shapes);var entity=new Probe(types[index],level,(float)steps[index]);entity.setPos(scenario.position);onGround.setBoolean(entity,scenario.grounded);
            Vec3 displacement=(Vec3)collide.invoke(entity,scenario.movement);
            JsonObject row=new JsonObject();row.addProperty("name",types[index].toString()+"/"+scenario.name);row.addProperty("kind",index);row.addProperty("step",steps[index]);row.addProperty("floor",scenario.floor);row.addProperty("grounded",scenario.grounded);row.add("position",numbers(scenario.position.x,scenario.position.y,scenario.position.z));row.add("movement",numbers(scenario.movement.x,scenario.movement.y,scenario.movement.z));row.add("blocks",cells);row.add("expected",numbers(displacement.x,displacement.y,displacement.z));
            Vec3 clipped=(Vec3)clip.invoke(null,scenario.movement,entity.getBoundingBox(),shapes);row.add("clip_expected",numbers(clipped.x,clipped.y,clipped.z));
            cases.add(row);
        }
        JsonObject output=new JsonObject();output.addProperty("minecraft","26.3");output.addProperty("protocol",777);output.add("cases",cases);Files.writeString(Path.of(args[1]),new GsonBuilder().setPrettyPrinting().create().toJson(output)+"\n");System.out.println("Original Entity.collide verified "+cases.size()+" scenes");
    }
}
