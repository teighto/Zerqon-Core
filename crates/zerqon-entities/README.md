# Zerqon entity simulation

This crate implements bounded pig, cow and zombie simulation at a fixed 20 Hz.
Call `EntityWorld::tick` once per server tick. Positions describe the center of
the feet; velocities are blocks per tick. The caller allocates globally unique
positive entity IDs and UUIDs and supplies a synchronous `WorldQuery` view.

`spawn` checks identity, capacity and collisions. `tick` applies gravity,
swept full-block collisions, passive wandering, zombie pursuit and cooldown-based
attacks. It returns state changes for the caller to transmit. `AttackPlayer`
requests damage; the caller owns player health and must apply that action.
Damage to a mob goes through `damage`, which emits health and removal changes.

Natural spawning is opt-in: call `try_natural_spawn` when the server's spawn
policy permits it. It tries at most eight locations, respects the entity cap,
avoids nearby players and allocates an identity only after finding valid space.
`hostile_allowed` is a policy input; this crate does not infer daylight or light
levels. `nearby` uses 16-block spatial buckets and accepts radii up to 128 blocks.

`snapshot` includes tick count, random state, velocities and AI cooldowns.
Floating-point values serialize as their IEEE bit patterns so JSON round trips
do not alter the next simulation step. Pass decoded snapshots to `restore`
before use; it rejects invalid coordinates, health, duplicate identities and
unsupported schemas. File size limits and atomic file replacement belong to the
caller. Determinism is tested within the same target and build.

The current AI and collision rules are limited. There is no pathfinding, step-up,
partial-block collision shape, water movement, breeding, drops, entity pushing,
fall damage, equipment or full vanilla mob parity. Unloaded space must be handled
explicitly by `WorldQuery`; returning solid blocks prevents movement into it.

```sh
cargo test -p zerqon-entities
cargo clippy -p zerqon-entities --all-targets -- -D warnings
```
