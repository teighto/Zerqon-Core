import java.util.List;
import net.minecraft.world.level.Level;
import net.minecraft.world.phys.AABB;
import net.minecraft.world.phys.shapes.VoxelShape;
import net.minecraft.world.entity.Entity;

final class FixtureLevel extends Level {
    private final List<VoxelShape> shapes;
    private final net.minecraft.world.level.border.WorldBorder border=new net.minecraft.world.level.border.WorldBorder();
    FixtureLevel(net.minecraft.core.RegistryAccess registries,List<VoxelShape> shapes){
        super(null,Level.OVERWORLD,registries,registries.lookupOrThrow(net.minecraft.core.registries.Registries.DIMENSION_TYPE).getOrThrow(net.minecraft.world.level.dimension.BuiltinDimensionTypes.OVERWORLD),false,false,0,512);
        this.shapes=shapes;
    }
    @Override public net.minecraft.world.level.border.WorldBorder getWorldBorder(){return border;}
    @Override public List<VoxelShape> getEntityCollisions(Entity entity,AABB query){return List.of();}
    @Override public Iterable<VoxelShape> getBlockCollisions(Entity entity,AABB query){return shapes.stream().filter(shape -> net.minecraft.world.phys.shapes.Shapes.joinIsNotEmpty(shape,net.minecraft.world.phys.shapes.Shapes.create(query),net.minecraft.world.phys.shapes.BooleanOp.AND)).toList();}
    @Override public net.minecraft.world.level.entity.LevelEntityGetter<net.minecraft.world.entity.Entity> getEntities(){throw new AssertionError("Unexpected fixture call: getEntities");}
    @Override public int getSeaLevel(){throw new AssertionError("Unexpected fixture call: getSeaLevel");}
    @Override public java.lang.String gatherChunkSourceStats(){throw new AssertionError("Unexpected fixture call: gatherChunkSourceStats");}
    @Override public java.util.Collection<net.minecraft.world.entity.boss.enderdragon.EnderDragonPart> dragonParts(){throw new AssertionError("Unexpected fixture call: dragonParts");}
    @Override public java.util.List<? extends net.minecraft.world.entity.player.Player> players(){throw new AssertionError("Unexpected fixture call: players");}
    @Override public net.minecraft.core.Holder<net.minecraft.world.level.biome.Biome> getUncachedNoiseBiome(int argument0,int argument1,int argument2){throw new AssertionError("Unexpected fixture call: getUncachedNoiseBiome");}
    @Override public net.minecraft.world.TickRateManager tickRateManager(){throw new AssertionError("Unexpected fixture call: tickRateManager");}
    @Override public net.minecraft.world.attribute.EnvironmentAttributeSystem environmentAttributes(){throw new AssertionError("Unexpected fixture call: environmentAttributes");}
    @Override public net.minecraft.world.clock.ClockManager clockManager(){throw new AssertionError("Unexpected fixture call: clockManager");}
    @Override public net.minecraft.world.entity.Entity getEntity(int argument0){throw new AssertionError("Unexpected fixture call: getEntity");}
    @Override public net.minecraft.world.flag.FeatureFlagSet enabledFeatures(){throw new AssertionError("Unexpected fixture call: enabledFeatures");}
    @Override public net.minecraft.world.item.crafting.RecipeAccess recipeAccess(){throw new AssertionError("Unexpected fixture call: recipeAccess");}
    @Override public net.minecraft.world.level.chunk.ChunkSource getChunkSource(){throw new AssertionError("Unexpected fixture call: getChunkSource");}
    @Override public net.minecraft.world.level.saveddata.maps.MapItemSavedData getMapData(net.minecraft.world.level.saveddata.maps.MapId argument0){throw new AssertionError("Unexpected fixture call: getMapData");}
    @Override public net.minecraft.world.level.storage.LevelData.RespawnData getRespawnData(){throw new AssertionError("Unexpected fixture call: getRespawnData");}
    @Override public net.minecraft.world.scores.Scoreboard getScoreboard(){throw new AssertionError("Unexpected fixture call: getScoreboard");}
    @Override public net.minecraft.world.ticks.LevelTickAccess<net.minecraft.world.level.block.Block> getBlockTicks(){throw new AssertionError("Unexpected fixture call: getBlockTicks");}
    @Override public net.minecraft.world.ticks.LevelTickAccess<net.minecraft.world.level.material.Fluid> getFluidTicks(){throw new AssertionError("Unexpected fixture call: getFluidTicks");}
    @Override public void destroyBlockProgress(int argument0,net.minecraft.core.BlockPos argument1,int argument2){throw new AssertionError("Unexpected fixture call: destroyBlockProgress");}
    @Override public void explode(net.minecraft.world.entity.Entity argument0,net.minecraft.world.damagesource.DamageSource argument1,net.minecraft.world.level.ExplosionDamageCalculator argument2,double argument3,double argument4,double argument5,float argument6,boolean argument7,net.minecraft.world.level.Level.ExplosionInteraction argument8,net.minecraft.core.particles.ParticleOptions argument9,net.minecraft.core.particles.ParticleOptions argument10,net.minecraft.util.random.WeightedList<net.minecraft.core.particles.ExplosionParticleInfo> argument11,net.minecraft.core.Holder<net.minecraft.sounds.SoundEvent> argument12){throw new AssertionError("Unexpected fixture call: explode");}
    @Override public void playSeededSound(net.minecraft.world.entity.Entity argument0,double argument1,double argument2,double argument3,net.minecraft.core.Holder<net.minecraft.sounds.SoundEvent> argument4,net.minecraft.sounds.SoundSource argument5,float argument6,float argument7,long argument8){throw new AssertionError("Unexpected fixture call: playSeededSound");}
    @Override public void playSeededSound(net.minecraft.world.entity.Entity argument0,net.minecraft.world.entity.Entity argument1,net.minecraft.core.Holder<net.minecraft.sounds.SoundEvent> argument2,net.minecraft.sounds.SoundSource argument3,float argument4,float argument5,long argument6){throw new AssertionError("Unexpected fixture call: playSeededSound");}
    @Override public void sendBlockUpdated(net.minecraft.core.BlockPos argument0,net.minecraft.world.level.block.state.BlockState argument1,net.minecraft.world.level.block.state.BlockState argument2,int argument3){throw new AssertionError("Unexpected fixture call: sendBlockUpdated");}
    @Override public void setRespawnData(net.minecraft.world.level.storage.LevelData.RespawnData argument0){throw new AssertionError("Unexpected fixture call: setRespawnData");}
    @Override public void gameEvent(net.minecraft.core.Holder<net.minecraft.world.level.gameevent.GameEvent> argument0,net.minecraft.world.phys.Vec3 argument1,net.minecraft.world.level.gameevent.GameEvent.Context argument2){throw new AssertionError("Unexpected fixture call: gameEvent");}
    @Override public void levelEvent(net.minecraft.world.entity.Entity argument0,int argument1,net.minecraft.core.BlockPos argument2,int argument3){throw new AssertionError("Unexpected fixture call: levelEvent");}
}
