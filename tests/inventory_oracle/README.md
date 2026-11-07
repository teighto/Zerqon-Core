# Minecraft 26.3 inventory codec oracle

These tests run the original Minecraft 26.3 `STREAM_CODEC` implementations against inventory packet bodies. Java 25 and locally prepared official data are required:

```text
python scripts/prepare-data.py
python scripts/test-inventory-codec.py
python scripts/test-inventory-codec.py --fixtures .cache/inventory-rust-fixtures.json
python scripts/test-inventory-codec.py --export .cache/inventory-oracle/official.json --export-default-items
python scripts/test-inventory-codec.py --export .cache/inventory-oracle/equipment.json --export-equipment
python tests/inventory_oracle/test_oracle.py
```

`--java` and `--javac` select explicit JDK paths. The runner checks the prepared version/protocol and the original server JAR checksum. It does not download anything. Source lint warnings are errors; `classfile` lint is excluded because the official server distribution omits compile-time annotation dependencies. Upstream JOML may print its own Java 25 Unsafe deprecation warning.

Fixture structure:

```json
{
  "version": "26.3",
  "protocol": 777,
  "fixtures": [
    {
      "name": "creative_stone64",
      "kind": "creative_slot",
      "body_hex": "002440010000",
      "expected": {
        "slot": 36,
        "stack": {"item": "minecraft:stone", "count": 64, "added": {}, "removed": []}
      }
    }
  ]
}
```

Bodies exclude packet IDs and frame lengths. Supported kinds: `creative_slot`, `container_click`, `carried_slot`, `use_item_on`, `server_stack`, `content`, `slot`, `cursor`, `held_slot`, `player_inventory`, `equipment`. The oracle checks packet IDs separately against the official generated report, requires complete input consumption and compares decoded values with `expected`.

Equipment packet 104 uses `expected: {"entity_id":7,"slots":[{"slot":0,"stack":null},{"slot":1,"stack":null}]}`. Slots retain wire order and use the original enum ordinal (0 main hand, 1 offhand). The continuation flag is the slot byte's high bit; each entry contains an ordinary server ItemStack. An empty update list is not a valid packet; empty hand stacks are valid. Equipment fixtures are always independently checked but only added to exports with `--export-equipment`, keeping the default Rust item-codec corpus unchanged.

An empty stack is JSON `null`. Nonempty stacks use canonical item names, a count, an `added` map keyed by component names with original component CODEC JSON values, and a lexically sorted `removed` array. Hashed stacks have the same keys, but `added` values are signed 32-bit hashes. Changed slots are ordered by slot number. `use_item_on.hit` contains coordinates relative to the clicked block.

`expect_error: true` requires a real codec failure. Unknown fixture kinds and trailing packet bytes are errors in the oracle itself. Fixtures marked `rust_reject: true` describe deliberate stricter Rust rules: negative counts and unused bytes within a component payload are accepted by vanilla but need not be accepted by Zerqon. They are not evidence of a Rust parity bug.

The small committed fixtures cover empty/default items, scalar components, plain NBT text including Unicode, block state, item model, rarity, removals, hashed click predictions and packet envelopes. Export can additionally cover all 1,658 standard item IDs. It validates the exported JSON again, including explicit null fields. This proves codec interoperability; it does not prove gameplay, placement rules or complete support for all component types.

## Русский

Проверки используют оригинальные кодеки Minecraft 26.3. Входной JSON содержит тело пакета без ID и длины, название формата и ожидаемые значения. Пустой слот — `null`; компоненты представлены типизированным JSON, хэши предсказаний проверяются отдельно. Полное потребление байтов обязательно.

Команды выше проверяют примеры, принимают экспорт Rust или создают эталонный экспорт Java для всех стандартных предметов. Отдельные проверки подтверждают, что oracle обнаруживает неверный номер слота, лишние байты и ошибочное имя кодека. Бинарные данные и Java-классы остаются в `.cache/`.

Различия строгого Rust-декодера помечены `rust_reject`: vanilla принимает отрицательный count как пустой слот и допускает неиспользованные байты внутри компонента. Совместимость кодеков не означает реализацию всех игровых механик или всех типов компонентов.
