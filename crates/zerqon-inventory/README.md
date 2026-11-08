# Zerqon inventory

Minecraft 26.3 item decoding, player inventory transitions and personal 2×2 crafting in Rust. World placement, equipment rules and item entities need their own services.

`ItemCatalog::load(data_directory)` reads the prepared version identity, numeric item/component registries and the official `generated/reports/<namespace>/components/item/*.json` defaults. Reads have per-file and aggregate limits. All 1,658 default items can be transported without serializing their inherited components. Item-specific bundle and equipment behavior is detected from actual defaults.

Item stacks own an immutable `Arc<ValidatedPatch>`, so inventory transactions and count changes share component data. Counts must be 1–99 and no greater than the item's effective maximum. Empty stacks use `None`. `ItemCatalog::same_kind` compares effective components, so an explicit patch equal to an item's default does not prevent stacking.

## Supported component patches

| Components | Representation |
| --- | --- |
| `max_stack_size`, `max_damage`, `damage`, `repair_cost` | Validated integer ranges |
| `unbreakable` | Unit marker |
| `custom_name`, `item_name` | Plain NBT strings, including modified UTF-8 Unicode |
| `item_model` | Validated identifier |
| `rarity`, `enchantment_glint_override` | Validated enum / boolean |
| `block_state` | Bounded string map; placement must additionally validate properties against the target block |

These are 11 of the version's 122 component types. Other added or removed components return `UnsupportedComponent`; rich text compounds, nested items and arbitrary NBT are not treated as valid opaque bytes. Default item components remain inherited from the official catalog. Future component implementations need typed validation and independent official-codec fixtures.

The creative client format has a byte-length prefix for each added component. Server item stacks do not. Separate encoders preserve that distinction. Parsers require complete consumption, reject duplicate/overlapping component IDs and enforce byte/count limits. Negative counts and unused bytes inside component payloads are deliberately rejected even though vanilla normalizes or ignores them.

## API

- `decode_creative_slot`, `decode_click`, `decode_carried_slot`, `decode_use_on`, `decode_player_action` consume packet **bodies**, without packet IDs or frame lengths.
- `decode_client_stack` and `decode_server_stack` expose the two stack encodings.
- `encode_*_body` functions and `encode_server_stack` produce bodies only. The caller provides the correct packet ID and framing.
- `PlayerInventory` owns 46 menu slots, cursor, selected hotbar slot, 15-bit state ID and bounded drag state.
- `creative_set` and `click` validate before committing. Errors leave all inventory state unchanged.
- `InventoryEffects` includes authoritative changed slots, cursor, state ID and explicit drops. `force_full_sync` currently requests a complete correction; client hashes never supply authoritative items.

Implemented click modes: pickup, quick move, hotbar/offhand swap, creative clone, throw, quick craft and pickup all. Generic inventory transitions conserve items across slots, cursor and returned drop effects. The legacy `click` API does not access slots 0–8; the personal crafting API below adds the grid and result. Armor rules, bundle click hooks and equipment auto-placement remain unsupported. The caller must verify player mode/permissions and pass `InventoryPolicy`; packet data is not an authority for those flags.

## Personal crafting

`CraftingRecipes::from_json` compiles ordinary shaped and shapeless definitions with item tags. It reports supported and excluded recipes. The pinned vanilla corpus has 478 supported recipes for the 2×2 grid; special recipes, larger patterns, nonempty output component patches and unsupported item hooks are excluded. Output counts require positive JSON integers, even though Java's codec accepts and truncates some floating-point values.

Pass a recipe book and `CraftingRemainders` to `click_with_crafting`; use `creative_set_with_crafting` for creative slot updates and `refresh_crafting` after loading owned items. `close_crafting` returns the cursor and real grid contents to inventory, then clears the result preview. Result slot 0 is derived and must not be persisted as an owned item. The runtime saves schema 2 without it and recalculates it after loading.

All actions compute a candidate before commit. Repeated crafting, ingredient consumption, output placement and remainders belong to the same transition. Errors leave the original slots, cursor, revision and drag unchanged, including failures in a later batch. With `allow_drops = true`, callers receive the original drop intentions and must commit them to real item entities. With drops disabled, an action that requires them is rejected atomically. The current server keeps overflowing grid/cursor items saved when closing a session instead of deleting them.

Dropping is disabled by default. Enable it only when the caller can commit the returned drops as real item entities together with the inventory change; discarding drop effects loses items. World placement and persistent player saves require their own transaction boundaries.

`decode_player_action` covers the nine 26.3 actions, including `ChangeDestroyDirection` at ordinal 1. It reads a byte direction; `decode_use_on` reads a VarInt face. Both preserve signed packed positions and reject negative sequences or trailing bytes. Directions outside 0–5 and boolean bytes other than 0/1 are deliberately rejected. Use-on hit coordinates use vanilla's tolerance around the block center, allowing values slightly outside 0–1. World height, reach, permissions and action ordering remain the caller's responsibility.

## Verification

```text
cargo test -p zerqon-inventory --locked
python scripts/test-inventory-codec.py --export .cache/inventory-oracle/official.json --export-default-items
cargo test -p zerqon-inventory --test official_oracle -- --ignored
python scripts/test-inventory-codec.py --fixtures .cache/inventory-rust-fixtures.json
cargo clippy -p zerqon-inventory --all-targets --locked -- -D warnings
python tools/crafting-data/generate.py --check
python tools/crafting-menu-oracle/run.py --check
```

The optional oracle tests require prepared official 26.3 data and Java 25. They cover every standard item plus typed component and packet fixtures in both Java→Rust and Rust→Java directions. Property tests cover Unicode encoding, truncation/malformed input, rollback and item conservation. Codec interoperability does not establish complete gameplay compatibility.

`block_actions` includes nine action and six use-on fixtures encoded by the original 26.3 server codec (server bundle SHA-1 `33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c`). They run without Java or downloaded game data. Additional tests cover packed-coordinate boundaries, malformed packets and hit-coordinate tolerance.

The crafting tests compare matching and parsing against original Java codecs and 34 menu traces captured from a private vanilla server. They include result pickup, shift, swaps, remainders, drops, closing and repeated stale revisions. Four floating-point count cases record the intentional stricter Rust input rule separately. Network tests run the actual Zerqon process through crafting, logout and restart.

## Русский

Крейт разбирает предметы Minecraft 26.3 и выполняет операции инвентаря на сервере. Все 1 658 стандартных предметов поддерживаются с их исходными компонентами из официальных данных; изменения компонентов реализованы для 11 типов из 122. Остальные типы и сложные NBT-формы явно отклоняются, а не пересылаются клиентам без проверки.

Есть 46 слотов, курсор, hotbar, creative updates и основные режимы клика. Изменения применяются только после проверки; ошибочные действия сохраняют прежнее состояние. Клиентские хэши используются только как предсказания и не создают предметы. Компоненты разделяются через `Arc`, без глубокого копирования при каждом клике.

Личная сетка 2×2 поддерживает 478 обычных рецептов с тегами, остатками, Shift-крафтом, обменом с хотбаром и закрытием меню. Для неё используются `click_with_crafting`, `creative_set_with_crafting`, `refresh_crafting` и `close_crafting`; старый `click` сохраняет прежний доступ только к обычным слотам. Выходной слот 0 вычисляется из ингредиентов и не сохраняется как отдельный предмет. Ошибка откатывает всю операцию, включая предыдущие партии одного Shift-крафта.

Dropping по умолчанию запрещён: вызывающий код обязан сохранить возвращённые drop effects вместе с реальными сущностями предметов. Текущий сервер при переполнении сохраняет сетку и курсор для следующего входа. Специальные рецепты, оборудование, bundle hooks, размещение и предметы в мире требуют дальнейшей реализации. Количество результата задаётся положительным JSON-целым; четыре отличия от Java при дробных значениях зафиксированы отдельно. Проверки используют оригинальные кодеки, 34 сценария настоящего vanilla-меню и сетевой запуск Zerqon.

`decode_player_action` разбирает все девять действий 26.3, включая новое действие смены направления под номером 1. Декодеры проверяют формат пакета; высоту мира, расстояние, права и применение изменений проверяет сервер. В тестах сохранены небольшие пакеты, полученные от оригинального кодека 26.3.
