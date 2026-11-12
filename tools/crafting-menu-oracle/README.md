# Minecraft 26.3 crafting-menu runtime oracle

This harness connects a small protocol-777 TCP client to the **unmodified original dedicated server** and exercises its real `InventoryMenu` / `ResultSlot` paths. No menu implementation, fake player or fake level is substituted.

```text
python tools/crafting-menu-oracle/run.py --accept-eula
python tools/crafting-menu-oracle/run.py --accept-eula --check
python tools/crafting-menu-oracle/test_oracle.py --report PATH_PRINTED_BY_RUNNER/report.json
```

Read the [Minecraft EULA](https://aka.ms/MinecraftEULA) before supplying `--accept-eula`. Java 25 and the existing official cache are required. `--java`, `--javac`, and `--data` select explicit executables/cache paths. For example in PowerShell:

```powershell
python tools/crafting-menu-oracle/run.py --accept-eula --check --java 'C:/Program Files/Java/jdk-25.0.4/bin/java.exe' --javac 'C:/Program Files/Java/jdk-25.0.4/bin/javac.exe'
```

The runner verifies the existing bundle, nested server and every classpath library using the same pinned checksums as `tools/crafting-data`. `MenuFacts.java` reads item IDs and default limits from those original classes afresh, so unverified generated reports cannot substitute item identities. Only this small helper is compiled. The actual server classpath contains the original server and verified libraries; no modified server classes or copied runtime are used. Upstream JOML may emit its Java 25 Unsafe deprecation warning.

Each run creates a new flat world under `.cache/crafting-menu-oracle/run-*`, binds only `127.0.0.1` on an ephemeral port, disables online authentication/whitelisting for the isolated test, and limits the Java heap to 768 MiB. No existing world or service is touched. The client is synthetic and uses no account credentials. Logs, the exact fixture snapshot, live item facts and raw packet evidence remain in that run directory. The server is stopped and reaped even on assertion failure. Successful output requires exit code 0, no forced kill, and a stopped log-reader thread.

## What is exercised

The 34 cases cover:

- Left/right result pickup, compatible/incompatible/full/partly full cursor, and shift result pickup with full or partial destination capacity.
- Ordinary crafting and result clone in Creative, plus rejected clone in Survival.
- Result swap to the hotbar/offhand and refusal with an occupied hotbar destination.
- Both result throw buttons, including repeated batch crafting; pickup-all from real inventory/grid slots while excluding the virtual result.
- Grid drag over two input slots with the result slot ignored.
- Honey-bottle remainders returning to the grid, merging into inventory, and overflowing into item entities; selected-slot/offhand/main-stack/empty-hotbar ordering.
- Closing with grid/cursor contents, selected-stack merging, and full-inventory drops in both game modes.
- Two repeated result clicks using the exact same stale `state_id`, each consuming another available ingredient batch.

Setup uses original creative-slot packets, then a console game-mode change. The action itself is a real container packet in the requested mode. A virtual recipe result is never inserted as a real inventory item by the harness.

The observations retain the full vanilla effects. For example, shift crafting with room for only two of four planks consumes one log, inserts two planks and drops two. Creative honey overflow discards the bottle remainder, while closing a full Creative inventory still drops cursor/grid items. A temporary Zerqon `allow_drops=false` policy may reject some operations, but that policy does not change this oracle's expected original behavior or the eventual drop-parity target.

## Snapshot synchronization and evidence

An ordinary observation requests a full authoritative snapshot using a stale-state, slot `-1` no-op click. A following `CHANGE_DESTROY_DIRECTION` packet (ordinal 1) at distant air `(0,319,0)` carries a unique sequence. Its `block_changed_ack` is handled on the same original packet-processing lane and fences the preceding menu work. The client processes all earlier full/delta snapshots until that numbered acknowledgement arrives. This matters because a stale result click produces its own full snapshot, in addition to the requested observation. A command response or network ping alone is not used as a menu barrier. This fence is for menu tests; it must not be inserted into a mining sequence. The actual `ABORT_DESTROY_BLOCK` ordinal is 2.

During intermediate quick-craft drag stages the harness sends only the numbered block-action/ACK fence and uses original authoritative deltas. A no-op menu click between drag stages would cancel the real drag; the harness deliberately avoids that. After the ending stage it requests a full snapshot again. Two initial observation requests are also checked to leave contents unchanged.

Each run's `report.json` contains per-case selected packet bodies, the step boundary indexes and observed state IDs. Packet logs are copied when the case completes, so subsequent cleanup cannot modify earlier evidence. `test_oracle.py` replays those clientbound packets up to every ACK and checks every recorded after-state, including drag stages. Item drops are read from original console `data get entity ... Item` output and aggregated by item name because vanilla may merge nearby item entities.

## File formats

`fixtures.json` contains explicit expected observations, originally captured from scoped exploratory original-server runs and then required on every repeat. It has schema 1, Minecraft/protocol identity, and 1..48 named cases. Each case contains:

- `gamemode`: `survival` or `creative`.
- `setup`: four `grid` inputs; `inventory` keyed by menu slot; `cursor`; and `selected` hotbar index 0..8.
- `actions`: a close action or a click with `slot`, `button`, integer `mode`, and `state_policy` (`current` or `same_stale`). Mode numbers are the original `ContainerInput` ordinals. Drag uses ordinary mode-5 stage packets; `-999` is allowed only for drag boundaries.
- `expected_before`, `expected_after`: sparse slot maps, cursor and selected index; omitted slots are empty. There is one after-state per action.
- `expected_dropped`: aggregate original item-entity counts.

Stacks use `{item, count, components:{}}`; null means empty. This corpus supports default stacks only and explicitly rejects unexpected component patches while decoding. Each case is bounded to eight actions; fixture and canonical JSON are bounded to 1 MiB, frames to the existing protocol helper's 2 MiB limit, and raw run evidence to 16 MiB. Network phases have hard deadlines. `--fixtures` and `--output` allow isolated checks without changing committed fixtures.

`oracle.json` is the deterministic offline corpus. It records source checksums, fixture SHA-256, original item IDs/limits, and cases with `before` / `after` snapshots containing **all 46 slots**, cursor and selected index. `actions`, `gamemode`, and aggregate `dropped` retain their meanings above. The raw revision IDs, timestamps, ephemeral port and packet timings belong only to per-run evidence. `--check` reruns the original server, validates every expectation and compares the canonical bytes without modifying the committed corpus.

These traces establish the listed menu behaviors on the pinned server. They do not establish all recipes, all item components, every drag mode, recipe-book commands, player-save durability, reconnect behavior or Zerqon's TCP/runtime integration. Those remain separate tests.
