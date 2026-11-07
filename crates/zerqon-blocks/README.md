# zerqon-blocks

Minecraft 26.3 block facts and creative building rules, written in Rust. This crate calculates edits; it does not modify a world or send packets.

The embedded 127 KiB asset contains 1,286 blocks, 35,723 states, 1,658 items and 338 shared collision shapes. State properties use compact numeric codes. Loading checks the compressed SHA-256, expansion size, version, references, property tables and geometry. It needs no external files, Java, Python or compiler at runtime.

`Catalog::item_block` uses the original item-to-block mapping. `State::properties` provides the reverse state lookup. `Catalog::resolve` and `with_properties` resolve named states. `State::collision` returns static boxes and rejects dynamic geometry. `Item::can_destroy_in_creative` is `None` when the item has a special destruction method that has not been ported.

`plan_placement` supports ordinary BlockItem instances for exact Block, RotatedPillarBlock, SlabBlock, StairBlock, SnowyBlock, GrassBlock and MyceliumBlock classes. Logs follow the clicked face. Slabs choose the correct half and merge. Stairs choose inner/outer corners and update nearby stairs. Grass, podzol and mycelium update their snowy property from the block above. `plan_break` checks the held item's creative destruction policy and recalculates neighbors.

Survival, fluids, dynamic geometry, interactive blocks, block entities, special items and unsupported neighbor effects return errors. Wet placement is rejected. Grass/mycelium spreading and decay, random ticks, bonemeal and snow-layer placement are not implemented. Block-state patches can change supported properties, but cannot enable waterlogging or force a stair shape. These restrictions are deliberate; this crate does not implement all vanilla block behavior.

## Applying a plan

Supply a stable `BlockView`; return `None` for unavailable data. Populate `Interaction` from server state: eye position, range attribute, world border/height and all relevant blocking entities. `Placement::item` and property overrides must come from the authoritative held stack. Check permissions, game mode, protection, teleport/loading state and plugin cancellation before committing.

An `EditPlan` contains every block read and each requested change. Lock the affected chunks in a consistent order, validate **all** read preconditions, and recheck live inventory, actor positions and authorization. Retry planning if the snapshot changed. Persist the entire edit transaction before publishing it. Cross-chunk plans require recoverable atomic persistence; sequential chunk saves do not satisfy this contract. Send corrections and block acknowledgements in commit order.

The planner does not consume items, emit sounds, update lighting, run scheduled ticks, raise plugin events or apply persistence. Those remain the caller's responsibility. Empty plans still describe processed actions and may require a client acknowledgement.

## Checks

```sh
cargo test -p zerqon-blocks
cargo clippy -p zerqon-blocks --all-targets -- -D warnings
python tools/block-data/generate.py --check
```

Tests include every reverse state lookup, 384 stair cases and 180 snowy-neighbor cases evaluated by the original server, placement/breaking, collision, bounds, missing chunks, unsupported effects, integrity failures and translation properties. The exporter and its requirements are described in [tools/block-data](../../tools/block-data/README.md).

## По-русски

Библиотека хранит данные блоков 26.3 и рассчитывает изменения при строительстве в творческом режиме. Поддержаны обычные блоки, брёвна, сухие плиты и лестницы, а также свойство снега у дёрна, подзола и мицелия. Распространение травы, случайные тики и снежные слои ещё не реализованы. Сервер должен сам проверить права, заблокировать нужные чанки, повторно проверить исходные данные и сохранить изменения целиком. Только после этого можно отправлять обновления игрокам.

Данные встроены в Rust-библиотеку. Java и Python нужны только разработчику для их повторной генерации. Выживание, жидкости и специальные механики пока отклоняются явно. Подключение к игровому серверу и сохранение миров в эту библиотеку не входят.
