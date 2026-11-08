import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonNull;
import com.google.gson.JsonObject;
import com.mojang.serialization.JsonOps;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.zip.ZipFile;
import net.minecraft.core.Holder;
import net.minecraft.core.HolderLookup;
import net.minecraft.core.component.DataComponentPatch;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.Identifier;
import net.minecraft.tags.TagFile;
import net.minecraft.tags.TagKey;
import net.minecraft.tags.TagLoader;
import net.minecraft.world.item.Item;
import net.minecraft.world.item.ItemStack;
import net.minecraft.world.item.crafting.CraftingInput;
import net.minecraft.world.item.crafting.CraftingRecipe;
import net.minecraft.world.item.crafting.Recipe;
import net.minecraft.world.item.crafting.ShapedRecipe;
import net.minecraft.world.item.crafting.ShapelessRecipe;

/** Recipe-level oracle only: it does not simulate InventoryMenu or ResultSlot clicks. */
public final class CraftingRecipeOracle {
    private static void require(boolean condition, String message) {
        CraftingDataExporter.require(condition, message);
    }

    private static int integer(JsonElement value, int minimum, int maximum, String name) {
        require(value != null && value.isJsonPrimitive() && value.getAsJsonPrimitive().isNumber()
            && value.getAsString().matches("0|[1-9][0-9]*"), "Invalid integer " + name);
        int number = Integer.parseInt(value.getAsString());
        require(number >= minimum && number <= maximum, "Out-of-range " + name);
        return number;
    }

    private static Identifier identifier(String value) {
        require(value.matches("minecraft:[a-z0-9_./-]+") && !value.contains(".."), "Invalid Minecraft identifier: " + value);
        return Identifier.parse(value);
    }

    private static int bindItemTags(ZipFile jar) throws Exception {
        String prefix = "data/minecraft/tags/item/";
        Map<Identifier, List<TagLoader.EntryWithSource>> definitions = new LinkedHashMap<>();
        for (var path : jar.stream().map(entry -> entry.getName())
                .filter(name -> name.startsWith(prefix) && name.endsWith(".json")).sorted().toList()) {
            String key = "minecraft:" + path.substring(prefix.length(), path.length() - 5);
            var file = TagFile.CODEC.parse(JsonOps.INSTANCE, CraftingDataExporter.json(jar, path)).getOrThrow();
            definitions.put(identifier(key), file.entries().stream()
                .map(entry -> new TagLoader.EntryWithSource(entry, "pinned-server-26.3")).toList());
        }
        require(definitions.size() == 236, "Unexpected original item tag count");
        TagLoader<Holder<Item>> loader = new TagLoader<>((key, required) -> BuiltInRegistries.ITEM.get(key), prefix);
        var resolved = loader.build(definitions);
        require(resolved.size() == definitions.size(), "Original item tags did not all resolve");
        Map<TagKey<Item>, List<Holder<Item>>> tags = new LinkedHashMap<>();
        resolved.forEach((key, entries) -> tags.put(TagKey.create(Registries.ITEM, key), entries));
        BuiltInRegistries.ITEM.prepareTagReload(new TagLoader.LoadResult<>(Registries.ITEM, tags)).apply();
        // These nested tags are not directly used by the ordinary 2x2 recipe sample.
        checkTag("logs", 48, "oak_log", "crimson_stem");
        checkTag("logs_that_burn", 40, "oak_log", "stripped_oak_wood");
        checkTag("oak_logs", 4, "oak_log", "stripped_oak_wood");
        require(!item("minecraft:crimson_stem").is(TagKey.create(Registries.ITEM, identifier("minecraft:logs_that_burn"))),
            "Nested tag binding included crimson_stem in logs_that_burn");
        return definitions.size();
    }

    private static Holder.Reference<Item> item(String key) {
        return BuiltInRegistries.ITEM.get(identifier(key))
            .orElseThrow(() -> new IllegalArgumentException("Unknown input item: " + key));
    }

    private static void checkTag(String name, int count, String first, String second) {
        var tag = TagKey.create(Registries.ITEM, identifier("minecraft:" + name));
        require(BuiltInRegistries.ITEM.get(tag).orElseThrow().size() == count
                && item("minecraft:" + first).is(tag) && item("minecraft:" + second).is(tag)
                && !item("minecraft:stone").is(tag), "Item tag binding differs: " + name);
    }

    private static ItemStack input(JsonElement value, HolderLookup.Provider lookup) {
        if (value.isJsonNull()) return ItemStack.EMPTY;
        if (value.isJsonPrimitive() && value.getAsJsonPrimitive().isString()) {
            return new ItemStack(item(value.getAsString()));
        }
        require(value.isJsonObject(), "Input stack must be an item name, object or null");
        var object = value.getAsJsonObject();
        require(object.keySet().equals(java.util.Set.of("item", "count", "components")), "Unexpected input stack fields");
        var stack = new ItemStack(item(object.get("item").getAsString()), integer(object.get("count"), 1, 99, "count"));
        stack.applyComponents(DataComponentPatch.CODEC.parse(lookup.createSerializationContext(JsonOps.INSTANCE), object.get("components")).getOrThrow());
        return stack;
    }

    private static JsonElement stack(ItemStack value, HolderLookup.Provider lookup) {
        if (value.isEmpty()) return JsonNull.INSTANCE;
        JsonObject result = new JsonObject();
        result.addProperty("item", BuiltInRegistries.ITEM.getKey(value.getItem()).toString());
        result.addProperty("count", value.getCount());
        result.add("components", DataComponentPatch.CODEC.encodeStart(lookup.createSerializationContext(JsonOps.INSTANCE), value.getComponentsPatch()).getOrThrow());
        return result;
    }

    private static void referencedTags(JsonElement value, java.util.Set<String> tags) {
        if (value.isJsonPrimitive() && value.getAsJsonPrimitive().isString()) {
            if (value.getAsString().startsWith("#")) tags.add(value.getAsString().substring(1));
        } else if (value.isJsonArray()) {
            for (var child : value.getAsJsonArray()) referencedTags(child, tags);
        } else if (value.isJsonObject()) {
            for (var child : value.getAsJsonObject().entrySet()) referencedTags(child.getValue(), tags);
        }
    }

    private static void referencedItems(JsonElement value, java.util.Set<String> items) {
        if (value.isJsonPrimitive() && value.getAsJsonPrimitive().isString()) {
            String key = value.getAsString();
            // Free-form group/component text may begin with "minecraft:" without being an identifier.
            if (key.matches("minecraft:[a-z0-9_./-]+") && !key.contains("..")
                    && BuiltInRegistries.ITEM.containsKey(Identifier.parse(key))) items.add(key);
        } else if (value.isJsonArray()) {
            for (var child : value.getAsJsonArray()) referencedItems(child, items);
        } else if (value.isJsonObject()) {
            for (var child : value.getAsJsonObject().entrySet()) referencedItems(child.getValue(), items);
        }
    }

    private static JsonObject observe(CraftingRecipe recipe, JsonArray grid, HolderLookup.Provider lookup) {
        require(grid.size() == 4, "Recipe input must contain exactly four row-major 2x2 slots");
        List<ItemStack> inputs = new ArrayList<>();
        JsonArray before = new JsonArray();
        for (var value : grid) {
            var parsed = input(value, lookup);
            inputs.add(parsed);
            before.add(stack(parsed, lookup));
        }
        var positioned = CraftingInput.ofPositioned(2, 2, inputs);
        var crafting = positioned.input();
        boolean matches = recipe.matches(crafting, null); // Ordinary recipe implementations do not consult Level.
        JsonObject observed = new JsonObject();
        observed.addProperty("matches", matches);
        if (matches) {
            observed.add("result", stack(recipe.assemble(crafting), lookup));
            var remaining = recipe.getRemainingItems(crafting);
            require(remaining.size() == crafting.size(), "Original recipe returned wrong remainder count");
            JsonElement[] physical = {JsonNull.INSTANCE, JsonNull.INSTANCE, JsonNull.INSTANCE, JsonNull.INSTANCE};
            for (int row = 0; row < crafting.height(); row++) for (int column = 0; column < crafting.width(); column++) {
                int index = (row + positioned.top()) * 2 + column + positioned.left();
                physical[index] = stack(remaining.get(row * crafting.width() + column), lookup);
            }
            JsonArray remainders = new JsonArray();
            for (var value : physical) remainders.add(value);
            observed.add("remainders", remainders);
        }
        JsonArray after = new JsonArray();
        for (var value : inputs) after.add(stack(value, lookup));
        require(before.equals(after), "Recipe query mutated the supplied input stacks");
        return observed;
    }

    private static JsonArray normalizedInput(JsonArray values, HolderLookup.Provider lookup, JsonObject components) {
        require(values != null && values.size() == 4, "Recipe input must contain exactly four row-major 2x2 slots");
        JsonArray normalized = new JsonArray();
        for (var value : values) {
            var parsed = input(value, lookup);
            normalized.add(stack(parsed, lookup));
            var patch = parsed.getComponentsPatch().split();
            patch.added().stream().forEach(component -> components.addProperty(
                BuiltInRegistries.DATA_COMPONENT_TYPE.getKey(component.type()).toString(),
                BuiltInRegistries.DATA_COMPONENT_TYPE.getId(component.type())));
            patch.removed().forEach(type -> components.addProperty(
                BuiltInRegistries.DATA_COMPONENT_TYPE.getKey(type).toString(), BuiltInRegistries.DATA_COMPONENT_TYPE.getId(type)));
        }
        return normalized;
    }

    private static JsonArray codecCases(ZipFile jar, JsonArray cases, HolderLookup.Provider lookup,
                                       JsonObject originals, JsonObject components, boolean strictRejections) throws Exception {
        require(cases.size() <= 64, "Expected at most 64 custom codec cases");
        JsonArray observations = new JsonArray();
        var names = new HashSet<String>();
        for (var entry : cases) {
            var test = entry.getAsJsonObject();
            var fields = strictRejections ? java.util.Set.of("name", "base_recipe", "overrides", "input", "expected", "rust_reject")
                : java.util.Set.of("name", "base_recipe", "overrides", "input", "expected");
            require(test.keySet().equals(fields),
                "Unexpected custom codec case fields");
            if (strictRejections) require(test.get("rust_reject").isJsonPrimitive()
                && test.get("rust_reject").getAsJsonPrimitive().isBoolean() && test.get("rust_reject").getAsBoolean(),
                "Strict rejection fixtures must explicitly set rust_reject: true");
            String name = test.get("name").getAsString();
            require(!name.isBlank() && name.length() <= 128 && names.add(name), "Invalid or duplicate custom codec case name");
            String base = test.get("base_recipe").getAsString();
            Identifier id = identifier(base);
            var original = CraftingDataExporter.json(jar, "data/minecraft/recipe/" + id.getPath() + ".json");
            var baseRecipe = Recipe.DIRECT_CODEC.parse(lookup.createSerializationContext(JsonOps.INSTANCE), original).getOrThrow();
            require(baseRecipe instanceof ShapedRecipe || baseRecipe instanceof ShapelessRecipe,
                "Custom codec cases require an original ordinary crafting recipe");
            originals.add(base, original);
            var recipe = original.deepCopy();
            var overrides = test.getAsJsonObject("overrides");
            require(!overrides.isEmpty() && java.util.Set.of("group", "category", "show_notification", "result").containsAll(overrides.keySet()),
                "Custom codec overrides may change only group, category, show_notification or result");
            overrides.entrySet().forEach(change -> recipe.add(change.getKey(), change.getValue()));
            var normalized = normalizedInput(test.getAsJsonArray("input"), lookup, components);
            var decoded = Recipe.DIRECT_CODEC.parse(lookup.createSerializationContext(JsonOps.INSTANCE), recipe);
            JsonObject observed = new JsonObject();
            boolean accepted = decoded.result().isPresent();
            require(!strictRejections || accepted, "Strict rejection fixtures must be accepted by the original codec");
            observed.addProperty("accepted", accepted);
            if (accepted) {
                var crafting = (CraftingRecipe) decoded.getOrThrow();
                observed.addProperty("show_notification", crafting.showNotification());
                observe(crafting, test.getAsJsonArray("input"), lookup).entrySet()
                    .forEach(value -> observed.add(value.getKey(), value.getValue()));
            }
            require(observed.equals(test.get("expected")), "Codec observation differs: " + name
                + " expected=" + test.get("expected") + " actual=" + observed);
            JsonObject result = new JsonObject();
            result.addProperty("name", name);
            result.addProperty("source", "custom_codec_variant");
            result.addProperty("base_recipe", base);
            result.add("recipe", recipe);
            result.add("input", normalized);
            result.add("expected", observed);
            if (strictRejections) result.addProperty("rust_reject", true);
            observations.add(result);
        }
        return observations;
    }

    static JsonObject verify(Path server, Path fixturePath, HolderLookup.Provider source) throws Exception {
        JsonObject fixture = CraftingDataExporter.json(fixturePath);
        require(integer(fixture.get("schema"), 1, 1, "schema") == 1
            && fixture.get("minecraft").getAsString().equals("26.3")
            && integer(fixture.get("protocol"), 777, 777, "protocol") == 777, "Wrong fixture version");
        var cases = fixture.getAsJsonArray("cases");
        require(cases != null && cases.size() >= 1 && cases.size() <= 512, "Expected 1..512 recipe cases");
        JsonArray observations = new JsonArray();
        JsonArray codecObservations;
        JsonArray strictRejections;
        JsonObject originalRecipes = new JsonObject();
        JsonObject originalTags = new JsonObject();
        JsonObject originalItems = new JsonObject();
        JsonObject originalComponents = new JsonObject();
        int tagCount;
        try (var jar = new ZipFile(server.toFile())) {
            tagCount = bindItemTags(jar);
            // Use the live item registry lookup after tags were bound; retain the original dynamic registries.
            var lookups = new ArrayList<HolderLookup.RegistryLookup<?>>();
            source.listRegistries().filter(registry -> !registry.key().equals(Registries.ITEM)).forEach(lookups::add);
            lookups.add(BuiltInRegistries.ITEM);
            var lookup = HolderLookup.Provider.create(lookups.stream());
            var names = new HashSet<String>();
            Map<String, CraftingRecipe> recipes = new TreeMap<>();
            for (var entry : cases) {
                var test = entry.getAsJsonObject();
                require(test.keySet().equals(java.util.Set.of("name", "recipe", "input", "expected")), "Unexpected recipe case fields");
                String name = test.get("name").getAsString();
                require(!name.isBlank() && name.length() <= 128 && names.add(name), "Invalid or duplicate case name");
                String key = test.get("recipe").getAsString();
                Identifier id = identifier(key);
                CraftingRecipe recipe = recipes.get(key);
                if (recipe == null) {
                    var json = CraftingDataExporter.json(jar, "data/minecraft/recipe/" + id.getPath() + ".json");
                    originalRecipes.add(key, json);
                    var decoded = Recipe.DIRECT_CODEC.parse(lookup.createSerializationContext(JsonOps.INSTANCE), json).getOrThrow();
                    require(decoded instanceof ShapedRecipe || decoded instanceof ShapelessRecipe, "Only ordinary shaped/shapeless recipes are supported by this oracle");
                    recipe = (CraftingRecipe) decoded;
                    recipes.put(key, recipe);
                }
                JsonObject observed = observe(recipe, test.getAsJsonArray("input"), lookup);
                require(observed.equals(test.get("expected")), "Recipe observation differs: " + name
                    + " expected=" + test.get("expected") + " actual=" + observed);
                JsonObject result = new JsonObject();
                result.addProperty("name", name);
                result.addProperty("recipe", key);
                result.add("input", normalizedInput(test.getAsJsonArray("input"), lookup, originalComponents));
                result.add("expected", observed);
                observations.add(result);
            }
            var codecInputs = fixture.has("codec_cases") ? fixture.getAsJsonArray("codec_cases") : new JsonArray();
            var strictInputs = fixture.has("strict_rejections") ? fixture.getAsJsonArray("strict_rejections") : new JsonArray();
            require(codecInputs.size() + strictInputs.size() <= 64, "Expected at most 64 total custom codec cases");
            codecObservations = codecCases(jar, codecInputs, lookup, originalRecipes, originalComponents, false);
            strictRejections = codecCases(jar, strictInputs, lookup, originalRecipes, originalComponents, true);
            var requiredTags = new TreeSet<String>();
            for (var entry : originalRecipes.entrySet()) {
                var definition = entry.getValue().getAsJsonObject();
                referencedTags(definition.has("key") ? definition.get("key") : definition.get("ingredients"), requiredTags);
            }
            for (var key : requiredTags) {
                var values = BuiltInRegistries.ITEM.get(TagKey.create(Registries.ITEM, identifier(key))).orElseThrow();
                JsonArray ids = new JsonArray();
                values.stream().map(holder -> BuiltInRegistries.ITEM.getKey(holder.value()).toString()).sorted().forEach(ids::add);
                originalTags.add(key, ids);
            }
            var requiredItems = new TreeSet<String>();
            referencedItems(originalRecipes, requiredItems);
            referencedItems(originalTags, requiredItems);
            referencedItems(observations, requiredItems);
            referencedItems(codecObservations, requiredItems);
            referencedItems(strictRejections, requiredItems);
            for (var key : requiredItems) {
                var item = item(key).value();
                var defaults = item.getDefaultInstance();
                JsonObject facts = new JsonObject();
                facts.addProperty("id", BuiltInRegistries.ITEM.getId(item));
                facts.addProperty("max_stack", defaults.getMaxStackSize());
                if (defaults.getMaxDamage() == 0) facts.add("max_damage", JsonNull.INSTANCE);
                else facts.addProperty("max_damage", defaults.getMaxDamage());
                originalItems.add(key, facts);
            }
        }
        JsonObject report = new JsonObject();
        report.addProperty("item_tags", tagCount);
        report.add("recipes", originalRecipes);
        report.add("tags", originalTags);
        report.add("items", originalItems);
        report.add("components", originalComponents);
        report.add("cases", observations);
        report.add("codec_cases", codecObservations);
        report.add("strict_rejections", strictRejections);
        return report;
    }
}
