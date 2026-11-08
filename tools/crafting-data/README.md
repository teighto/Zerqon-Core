# Original Minecraft 26.3 crafting facts

This tool queries the pinned original server classes with Java 25. It exports the item crafting remainders and a small recipe-level reference corpus. No Minecraft implementation is reproduced in the Java sources.

From the repository root, with the existing official cache prepared:

```text
python tools/crafting-data/generate.py
python tools/crafting-data/generate.py --check
python tools/crafting-data/test_crafting_data.py
```

Use `--java` and `--javac` for explicit Java 25 executable paths. For example in PowerShell:

```powershell
python tools/crafting-data/generate.py --check --java 'C:/Program Files/Java/jdk-25.0.4/bin/java.exe' --javac 'C:/Program Files/Java/jdk-25.0.4/bin/javac.exe'
```

`--data` defaults to `.cache/vanilla/26.3`. No downloads or server startup are performed. The runner streams and verifies the bundle SHA-1, nested server SHA-256, and every classpath library SHA-256 from the pinned bundle's library manifest. It uses only those verified libraries, excluding unrelated JARs in the cache. Temporary classes, logs, fixture snapshots and intermediate exports stay under `.cache/crafting-data` and are removed after each run. Java source warnings are errors; `classfile` lint is excluded because the upstream distribution omits some compile-time annotation dependencies. Upstream JOML prints its Java 25 Unsafe deprecation warning.

## Facts format

`26.3.json` has schema 1:

```json
{
  "schema": 1,
  "minecraft": "26.3",
  "protocol": 777,
  "data_version": 5023,
  "bundle_sha1": "33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c",
  "server_sha256": "a362163eec5d1612d520772bc16e5b39c09e3b234fdc045f56bf544284ee8ae6",
  "item_count": 1658,
  "remainders": [["minecraft:honey_bottle", "minecraft:glass_bottle", 1]]
}
```

The example shows one remainder; the generated file contains every non-null remainder. Each row is `[consumed_item_name, remainder_item_name, remainder_count]`, sorted by consumed item name. Missing rows mean the original `Item.getCraftingRemainder()` returned null. The exporter calls `ItemStackTemplate.create()` and rejects any remainder containing a component patch; schema 1 cannot silently discard such a patch. `item_count` and version metadata come from the original live registry/version, with explicit checks against the pinned version. `minecraft:use_remainder` is not used.

## Recipe oracle and offline corpus

`fixtures.json` contains explicit expectations checked against the original runtime. Each `cases` entry has `name`, `recipe`, `input`, and `expected`. `input` always contains four physical slots in row-major order (top-left, top-right, bottom-left, bottom-right). An empty slot is null. Input items may be a full item name (count 1, empty patch) or an object:

```json
{"item":"minecraft:honey_bottle","count":16,"components":{}}
```

Counts are bounded to 1..99. `components` uses the original `DataComponentPatch.CODEC` JSON format, including removal keys when present. A successful expectation contains:

```json
{
  "matches": true,
  "result": {"item":"minecraft:honey_block","count":1,"components":{}},
  "remainders": [
    {"item":"minecraft:glass_bottle","count":1,"components":{}},
    {"item":"minecraft:glass_bottle","count":1,"components":{}},
    {"item":"minecraft:glass_bottle","count":1,"components":{}},
    {"item":"minecraft:glass_bottle","count":1,"components":{}}
  ]
}
```

A nonmatching expectation is exactly `{"matches":false}`. The oracle does not call `assemble` or `getRemainingItems` for a nonmatch. It also checks that the original queries did not mutate any input stack.

Recipes and all 236 item tags are read directly from the verified server JAR. Original `TagFile.CODEC` and `TagLoader.build` resolve the tag definitions, then `prepareTagReload(...).apply()` binds them. Assertions verify nested logs tags, including positive membership, exclusions and counts. Original `Recipe.DIRECT_CODEC`, `CraftingInput.ofPositioned`, `matches`, `assemble` and `getRemainingItems` produce each observation. Only ordinary `ShapedRecipe`/`ShapelessRecipe` are accepted; passing null Level is confined to these implementations. Remainders from trimmed crafting input are mapped back to the original four physical slots.

The 23 cases cover offset and mirror, forbidden rotations, empty/extra/missing inputs, tag membership, item alternatives, shapeless order, repeated ingredients, stack count versus slot multiplicity, input components, honey bottle remainders and a remainder in an offset single slot. They cover eight recipe definitions; they are not an assertion that all ordinary recipes are supported by Zerqon. In particular, the 17 suspicious-stew component outputs are outside this corpus and remain unsupported by the core.

`codec_cases` separately checks 46 custom variants of the pinned shaped torch and shapeless oak-planks recipes. These variants are test inputs, not unchanged original JAR resources. Each input entry contains `name`, `base_recipe`, `overrides`, `input`, and `expected`. The base must be an existing original ordinary recipe. Overrides replace only the top-level `group`, `category`, `show_notification`, or `result` fields. The original base is decoded successfully before each variant is passed to `Recipe.DIRECT_CODEC` with the real registry context.

For rejection, `expected` is exactly `{"accepted":false}`. Missing base resources, unknown input items and malformed fixture structure fail the oracle itself; they cannot satisfy an expected codec rejection. For acceptance, `expected` includes `accepted: true`, `show_notification`, and the ordinary `matches`/`result`/`remainders` observation described above. The accepted variants are also matched and assembled using their input grid.

The codec cases confirm shorthand result strings with count 1, explicit empty output components, `show_notification: false` for both recipe kinds, and null optional metadata/result fields. Null `group`, `category`, `show_notification`, result `count`, and result `components` select the original defaults. Invalid object/numeric/boolean/string metadata types and invalid result forms are checked separately. Nonempty output-component support is not implied by these cases.

`strict_rejections` contains four separately labeled custom variants: result counts `1.0` and `1.5` for each recipe kind. The original codec accepts both and produces count 1. Zerqon's current recipe contract requires positive JSON integers and deliberately rejects these values. Each fixture retains the actual original acceptance and result, plus `rust_reject: true`; it is evidence of this narrow intentional difference, not full codec parity. A strict-rejection fixture must actually be accepted by the original codec.

`oracle.json` is generated only after every expectation has passed. It contains:

- The same schema/version/source provenance, plus the SHA-256 of the exact fixture snapshot used by Java.
- `recipes`: recipe ID to the original recipe JSON object.
- `tags`: referenced item tag ID to a sorted array of resolved item IDs. These are flattened facts, suitable for an offline matcher test.
- `cases`: the same case envelope, with every nonempty `input` normalized to the object form and `expected` populated from the actual original Java observation.
- `codec_cases`: custom variants with `source: "custom_codec_variant"`, `base_recipe`, the full changed `recipe` JSON, normalized `input`, and the actual codec/matching `expected` observation. These remain separate from `recipes` and `cases`, so a consumer can validate rejected definitions individually without poisoning the valid recipe book.
- `strict_rejections`: the same custom-variant structure, with `rust_reject: true` and the original accepted observation, for the four intentional numeric-format differences above.
- `items`: referenced item name to `{id, max_stack, max_damage}` from the original registry and default item stack. `max_damage` is null for a non-damageable item. This includes all resolved alternatives, inputs, outputs and remainders, for an offline test catalog.
- `components`: input-patch component name to its original registry ID.
- `item_tags`: the total number of loaded item tags, including tags not included in the compact export.

`--output` chooses the facts output; `--oracle-output` chooses the corpus output and defaults to `oracle.json` next to the facts file. `--fixtures` supplies a different expectation set; it must contain 1..512 ordinary cases and may contain up to 64 custom codec cases in total across `codec_cases` and `strict_rejections`. `--report` writes another copy of the verified corpus, normally under `.cache`. Output paths must be distinct and cannot overwrite input fixtures. JSON inputs and outputs are bounded to 1 MiB. The fixture is snapshotted before Java runs, so the provenance hash identifies the exact checked bytes. Generation is deterministic; `--check` reruns the original queries and requires exact bytes for both committed artifacts. It does not modify them.

These checks establish recipe matching, result construction and crafting remainder facts. They do not exercise `InventoryMenu`, `ResultSlot`, result clicks, shift-click, destination capacity, durable inventory commits, close/reconnect, or network behavior. Those require separate integration tests.
