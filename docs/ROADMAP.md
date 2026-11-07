# Zerqon implementation plan

Target: Minecraft Java Edition **26.3**, wire protocol **777**. The engine is written from scratch in Rust. Java runs optional server plugins and prepares the initial game data. World simulation runs in Rust. Version 0.0.1 is a development milestone until the acceptance gates below pass.

## Delivery gates

1. **Foundation**: reproducible Rust workspace, bounded protocol decoding, strict configuration, graceful shutdown, CI, release packaging. Verify malformed inputs, lifecycle and clean builds on Windows/Linux.
2. **Client connection**: status, authenticated login, configuration registries, play state, chunks, movement, keep-alive, disconnect. Verify against the official 26.3 client and Horizon; record exact builds. Offline mode is an explicit testing option.
3. **World storage and threading**: compact chunk palettes, deterministic generation, atomic persistence, bounded worker queues, ownership of regions and cross-region messages. Verify save/restart, corruption handling, concurrency and memory under sustained load.
4. **Playable world**: block interactions, inventories, entities, combat, survival, crafting, fluids, lighting, redstone, dimensions, generation, commands, permissions and datapacks. Track vanilla parity with executable differential tests, not advertised API names.
5. **Rust plugins**: versioned sandboxed ABI, lifecycle, commands, events, scheduling, permissions and world operations; SDK, examples and failure isolation. Verify real compiled Rust plugins, time/memory limits and restart behavior.
6. **Java/Paper plugins**: optional JVM bridge, classloading, lifecycle, configuration, commands/events/scheduler, inventory/world/entity APIs and services. Maintain a method-level support matrix. Unsupported methods fail explicitly. Verify real third-party plugins; never substitute successful no-ops. Preserve Paper's synchronous execution contract through an ownership barrier.
7. **Operations**: useful console and in-game commands, /tps, /tpsbar, profiling, spark integration, monitoring, backups, resource limits and secure defaults. Profiles must identify the runtime and threads actually sampled.
8. **Production gate**: full integration suite, vanilla/Horizon compatibility, realistic multi-client soak, memory/CPU profiles, crash recovery, security review and documented limitations. Publish measured results and only then designate a production release.

## Publication

Keep source, tests, essential documentation, reproducible build scripts and genuine screenshots. Exclude generated worlds, credentials, downloaded game assets, tool caches, local reports and binaries from Git. Release archives contain the native server and its plugin libraries. Preserve dependency licenses. Commit author: teighto. Release notes must describe only verified capabilities.

## План на русском

Цель — собственное ядро Rust для Java Edition 26.3: сеть → вход → мир и хранение → игровой процесс → Rust-плагины → совместимость Paper → диагностика и оптимизация → нагрузочные испытания и выпуск. Каждый этап принимается по работающим сценариям. Полная совместимость Paper и готовность к продакшену не следуют из успешной сборки или отдельного теста подключения. Ограничения и проверенные версии клиентов фиксируются явно.
