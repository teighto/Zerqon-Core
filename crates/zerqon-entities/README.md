# zerqon-entities

Bounded pig, cow and zombie simulation with persisted identities and movement. Terrain collision uses the static boxes from `zerqon-blocks`, including slabs, stairs and shapes that extend outside their block cell.

Call `EntityWorld::tick` once per server tick at 20 Hz. Positions describe the center of the feet; velocities are blocks per tick. The caller allocates globally unique positive entity IDs and UUIDs and supplies a synchronous `WorldQuery` view. `spawn` validates identity, capacity and space. `tick` returns changes for the caller to transmit. `AttackPlayer` requests damage; the caller owns player health. Mob damage goes through `damage`, which returns health and removal changes.

Natural spawning is opt-in through `try_natural_spawn`. Each call tries at most eight locations, respects the cap and distance from players, and allocates an identity after finding supported space. `hostile_allowed` is a caller policy, not a daylight or light-level calculation. `nearby` uses 16-block buckets and accepts radii up to 128 blocks.

`WorldQuery::block_collision` supplies empty space, a full cube, a borrowed static shape, unavailable terrain or unsupported physics. The default adapts existing `block_solid` implementations for worlds made of full cubes. The server's `ChunkView` uses the shape catalog directly. `surface_height` supports fractional surfaces; natural spawning also checks actual support before allocating an entity identity.

Movement sweeps the entire requested displacement. Vertical clipping runs first, followed by the larger horizontal component. Each block's shape stays grouped so the original epsilon rules are preserved. Grounded mobs can step up to their default 0.6-block limit when there is enough headroom. Scratch buffers are reused across mobs.

`move_mob` reads block-local geometry through `WorldQuery`. `clip_shapes` preserves supplied voxel-shape groups; `clip_motion` treats each supplied box as a separate collider. Static block boxes must fit the reviewed 26.3 local-coordinate range, -0.25 through 1.5.

Queries are limited to 4,096 block cells and 4,096 collision boxes, with at most 64 boxes per block. Movement inputs must be finite and at most 32 blocks per axis per call. Runtime gravity and movement use much smaller velocities.

Unavailable terrain, fluids and dynamic geometry stop the affected mob's physics. Its position, AI state and random state are preserved. A `PhysicsBlocked` event is emitted when the reason changes; restoring supported terrain lets it resume. These cases are not treated as air or full cubes.

`snapshot` retains tick count, random state, velocities and AI cooldowns. Floats serialize as IEEE bit patterns so a JSON round trip preserves the next simulation step. `restore` rejects invalid coordinates, health, duplicate identities and unsupported schemas. File size limits and atomic replacement belong to the caller. Determinism is tested within the same target and build.

The crate does not yet implement fluid travel, entity pushing, climbing, jumping pathfinding, breeding, drops, fall damage, equipment, block-specific friction/effects or complete vanilla AI. Collision accuracy does not imply that these mechanics exist.

## Checks

```sh
cargo test -p zerqon-entities --locked
python scripts/test-entity-physics.py
cargo test -p zerqon-entities --release measure_one_thousand_entities_on_flat_ground --locked -- --ignored --nocapture
```

The original-code fixtures are described in [tools/entity-physics](../../tools/entity-physics/README.md). The manual benchmark measures this simulation on a synthetic flat world; it is not a server player-capacity benchmark.

## По-русски

Свиньи, коровы и зомби учитывают настоящую форму блоков. Они приземляются на половину плиты, поднимаются по ступеням и не проходят сквозь препятствия при быстром движении. Низкий потолок мешает подъёму.

Если нужный чанк недоступен или движение требует ещё не реализованной физики жидкости либо динамического блока, моб останавливается до появления подходящих данных. Его состояние сохраняется. Плавание, толкание сущностей, полноценный поиск пути и остальные повадки vanilla ещё не реализованы.
