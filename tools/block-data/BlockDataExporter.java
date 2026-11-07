import com.google.gson.Gson;
import com.google.gson.JsonArray;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.util.zip.ZipFile;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import net.minecraft.SharedConstants;
import net.minecraft.core.BlockPos;
import net.minecraft.core.Direction;
import net.minecraft.core.Holder;
import net.minecraft.core.component.DataComponentInitializers;
import net.minecraft.core.component.DataComponents;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.core.registries.Registries;
import net.minecraft.data.registries.VanillaRegistries;
import net.minecraft.server.Bootstrap;
import net.minecraft.resources.Identifier;
import net.minecraft.tags.BlockTags;
import net.minecraft.tags.TagLoader;
import net.minecraft.util.RandomSource;
import net.minecraft.world.item.BlockItem;
import net.minecraft.world.item.context.BlockPlaceContext;
import net.minecraft.world.level.BlockGetter;
import net.minecraft.world.level.EmptyBlockGetter;
import net.minecraft.world.level.LevelReader;
import net.minecraft.world.level.ScheduledTickAccess;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.Blocks;
import net.minecraft.world.level.block.GameMasterBlock;
import net.minecraft.world.level.block.RotatedPillarBlock;
import net.minecraft.world.level.block.SlabBlock;
import net.minecraft.world.level.block.StairBlock;
import net.minecraft.world.level.block.SnowyBlock;
import net.minecraft.world.level.block.GrassBlock;
import net.minecraft.world.level.block.MyceliumBlock;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.level.block.state.properties.Half;
import net.minecraft.world.level.block.state.properties.Property;
import net.minecraft.world.level.block.state.properties.StairsShape;
import net.minecraft.world.level.material.FluidState;
import net.minecraft.world.phys.shapes.CollisionContext;

public final class BlockDataExporter {
    static String owner(Class<?> type, String name) {
        for (Class<?> current = type; current != null; current = current.getSuperclass()) {
            for (var method : current.getDeclaredMethods()) {
                if (method.getName().equals(name)) return current.getSimpleName();
            }
        }
        throw new IllegalStateException("Missing method: " + name);
    }

    static <T extends Comparable<T>> String value(BlockState state, Property<T> property) {
        return property.getName(state.getValue(property));
    }

    static int family(Block block) {
        if (block.getClass() == Block.class) return 1;
        if (block.getClass() == RotatedPillarBlock.class) return 2;
        if (block.getClass() == SlabBlock.class) return 3;
        if (block.getClass() == StairBlock.class) return 4;
        if (block.getClass() == SnowyBlock.class || block.getClass() == GrassBlock.class || block.getClass() == MyceliumBlock.class) return 5;
        return 0;
    }

    static final class View implements BlockGetter {
        final Map<BlockPos, BlockState> states;
        View(Map<BlockPos, BlockState> states) { this.states = states; }
        public BlockEntity getBlockEntity(BlockPos position) { return null; }
        public BlockState getBlockState(BlockPos position) { return states.getOrDefault(position, Blocks.AIR.defaultBlockState()); }
        public FluidState getFluidState(BlockPos position) { return getBlockState(position).getFluidState(); }
        public int getHeight() { return 384; }
        public int getMinY() { return -64; }
    }

    public static void main(String[] args) throws Exception {
        SharedConstants.tryDetectVersion();
        Bootstrap.bootStrap();
        BuiltInRegistries.DATA_COMPONENT_INITIALIZERS.build(VanillaRegistries.createWorldLookup())
            .forEach(DataComponentInitializers.PendingComponents::apply);
        List<Holder<Block>> snow = new ArrayList<>();
        try (var jar = new ZipFile(args[1]); var input = jar.getInputStream(jar.getEntry("data/minecraft/tags/block/snow.json"))) {
            var values = JsonParser.parseReader(new InputStreamReader(input, StandardCharsets.UTF_8)).getAsJsonObject().getAsJsonArray("values");
            for (var entry : values) snow.add(BuiltInRegistries.BLOCK.get(Identifier.parse(entry.getAsString())).orElseThrow());
        }
        BuiltInRegistries.BLOCK.prepareTagReload(new TagLoader.LoadResult<>(Registries.BLOCK, Map.of(BlockTags.SNOW, snow))).apply();
        if (!Blocks.SNOW.defaultBlockState().is(BlockTags.SNOW) || !Blocks.SNOW_BLOCK.defaultBlockState().is(BlockTags.SNOW) || Blocks.STONE.defaultBlockState().is(BlockTags.SNOW)) throw new AssertionError("Snow tag binding failed");
        JsonObject output = new JsonObject();
        JsonArray blocks = new JsonArray(), states = new JsonArray(), items = new JsonArray();
        for (var block : BuiltInRegistries.BLOCK) {
            JsonObject entry = new JsonObject();
            entry.addProperty("id", BuiltInRegistries.BLOCK.getId(block));
            entry.addProperty("key", BuiltInRegistries.BLOCK.getKey(block).toString());
            entry.addProperty("default", Block.getId(block.defaultBlockState()));
            entry.addProperty("family", family(block));
            entry.addProperty("item", BuiltInRegistries.ITEM.getId(block.asItem()));
            int flags = 0;
            if (block instanceof StairBlock) flags |= 1;
            if (!owner(block.getClass(), "useItemOn").equals("BlockBehaviour")
                    || !owner(block.getClass(), "useWithoutItem").equals("BlockBehaviour")) flags |= 2;
            if (!owner(block.getClass(), "canBeReplaced").equals("BlockBehaviour")) flags |= 4;
            if (!owner(block.getClass(), "neighborChanged").equals("BlockBehaviour")
                    || !owner(block.getClass(), "updateShape").equals("BlockBehaviour")) flags |= 8;
            if (block instanceof GameMasterBlock) flags |= 16;
            if (block.defaultBlockState().is(BlockTags.SNOW)) flags |= 32;
            entry.addProperty("flags", flags);
            blocks.add(entry);
        }
        for (var state : Block.BLOCK_STATE_REGISTRY) {
            JsonObject entry = new JsonObject();
            entry.addProperty("id", Block.getId(state));
            entry.addProperty("block", BuiltInRegistries.BLOCK.getId(state.getBlock()));
            int flags = 0;
            if (state.isAir()) flags |= 1;
            if (state.canBeReplaced()) flags |= 2;
            if (!state.getFluidState().isEmpty()) flags |= 4;
            if (state.hasBlockEntity()) flags |= 8;
            if (state.getBlock().hasDynamicShape()) flags |= 16;
            entry.addProperty("flags", flags);
            JsonObject properties = new JsonObject();
            state.getProperties().stream().sorted(Comparator.comparing(Property::getName))
                .forEach(property -> properties.addProperty(property.getName(), value(state, property)));
            entry.add("properties", properties);
            JsonArray boxes = new JsonArray();
            for (var box : state.getCollisionShape(EmptyBlockGetter.INSTANCE, BlockPos.ZERO, CollisionContext.empty()).toAabbs()) {
                JsonArray coordinates = new JsonArray();
                for (double number : new double[] { box.minX, box.minY, box.minZ, box.maxX, box.maxY, box.maxZ }) coordinates.add(number);
                boxes.add(coordinates);
            }
            entry.add("collision", boxes);
            states.add(entry);
        }
        for (var item : BuiltInRegistries.ITEM) {
            JsonObject entry = new JsonObject();
            entry.addProperty("id", BuiltInRegistries.ITEM.getId(item));
            entry.addProperty("key", BuiltInRegistries.ITEM.getKey(item).toString());
            entry.addProperty("block", item instanceof BlockItem block ? BuiltInRegistries.BLOCK.getId(block.getBlock()) : -1);
            entry.addProperty("ordinary", item.getClass() == BlockItem.class);
            var tool = item.getDefaultInstance().get(DataComponents.TOOL);
            entry.addProperty("creative_destroy", tool == null || tool.canDestroyBlocksInCreative());
            entry.addProperty("creative_destroy_supported", owner(item.getClass(), "canDestroyBlock").equals("Item"));
            items.add(entry);
        }
        output.add("blocks", blocks);
        output.add("states", states);
        output.add("items", items);
        var shapeMethod = StairBlock.class.getDeclaredMethod("getStairsShape", BlockState.class, BlockGetter.class, BlockPos.class);
        shapeMethod.setAccessible(true);
        JsonArray cases = new JsonArray();
        Direction[] directions = { Direction.NORTH, Direction.SOUTH, Direction.WEST, Direction.EAST };
        for (var facing : directions) for (var half : Half.values()) for (boolean front : new boolean[] { true, false })
            for (var neighborFacing : directions) for (var neighborHalf : Half.values()) for (int blocker = 0; blocker < 3; blocker++) {
                var base = Blocks.OAK_STAIRS.defaultBlockState().setValue(StairBlock.FACING, facing).setValue(StairBlock.HALF, half);
                var neighbor = Blocks.STONE_STAIRS.defaultBlockState().setValue(StairBlock.FACING, neighborFacing).setValue(StairBlock.HALF, neighborHalf);
                Map<BlockPos, BlockState> grid = new HashMap<>();
                grid.put(BlockPos.ZERO.relative(front ? facing : facing.getOpposite()), neighbor);
                var blockerPosition = BlockPos.ZERO.relative(front ? neighborFacing.getOpposite() : neighborFacing);
                if (blocker != 0) grid.put(blockerPosition, base.setValue(StairBlock.HALF, blocker == 1 ? half : (half == Half.TOP ? Half.BOTTOM : Half.TOP)));
                var shape = (StairsShape) shapeMethod.invoke(null, base, new View(grid), BlockPos.ZERO);
                JsonArray row = new JsonArray();
                row.add(Block.getId(base)); row.add(Block.getId(base.setValue(StairBlock.SHAPE, shape)));
                JsonArray neighbors = new JsonArray();
                List<BlockPos> positions = new ArrayList<>(grid.keySet());
                positions.sort(Comparator.comparingInt((BlockPos position) -> position.getX()).thenComparingInt(position -> position.getZ()));
                for (var position : positions) {
                    JsonArray n = new JsonArray(); n.add(position.getX()); n.add(position.getZ()); n.add(Block.getId(grid.get(position))); neighbors.add(n);
                }
                row.add(neighbors); cases.add(row);
            }
        output.add("stair_cases", cases);
        var snowMethod = SnowyBlock.class.getDeclaredMethod("updateShape", BlockState.class, LevelReader.class, ScheduledTickAccess.class, BlockPos.class, Direction.class, BlockPos.class, BlockState.class, RandomSource.class);
        snowMethod.setAccessible(true);
        JsonArray snowCases = new JsonArray();
        for (var block : new Block[] { Blocks.GRASS_BLOCK, Blocks.PODZOL, Blocks.MYCELIUM })
            for (boolean wasSnowy : new boolean[] { false, true }) for (var direction : Direction.values())
                for (var neighbor : new Block[] { Blocks.AIR, Blocks.STONE, Blocks.SNOW, Blocks.SNOW_BLOCK, Blocks.POWDER_SNOW }) {
                    var state = block.defaultBlockState().setValue(SnowyBlock.SNOWY, wasSnowy);
                    var changed = neighbor.defaultBlockState();
                    var expected = (BlockState) snowMethod.invoke(block, state, null, null, BlockPos.ZERO, direction, BlockPos.ZERO.relative(direction), changed, RandomSource.create(0));
                    JsonArray row = new JsonArray();
                    row.add(Block.getId(state)); row.add(direction.get3DDataValue()); row.add(Block.getId(changed)); row.add(Block.getId(expected));
                    snowCases.add(row);
                }
        output.add("snow_cases", snowCases);
        Files.writeString(Path.of(args[0]), new Gson().toJson(output));
    }
}
