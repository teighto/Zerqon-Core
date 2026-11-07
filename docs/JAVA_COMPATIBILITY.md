# Java plugins and profiling / Java-плагины и профилирование

## English

Target: Minecraft Java Edition **26.3**, Paper API 26.3, JVM **25**. The JAR launcher only starts the Rust executable. It is not a Java plugin host, a Paper compatibility layer, or a bundled spark implementation. `plugins/java-plugins/` is the reserved location for future Java server plugins; `plugins/rust-plugins/` is for Rust server plugins. These are server plugins, not client mods.

### Compatibility roadmap

| Stage | Required behavior and evidence |
| --- | --- |
| Runtime | Optional JVM adapter; game/network/world ownership remains in Rust. Pin the exact Paper API build and its dependencies under `libraries/`; use `.cache/` for downloads/cache. |
| Loading | Implement `ConfiguredPluginClassLoader`, plugin dependency ordering, lifecycle services, `plugin.yml` and `paper-plugin.yml`. Test a JAR compiled against the official API without recompiling it for Zerqon. |
| Basic APIs | Real logging, configuration, commands, permissions, scheduler and lifecycle; tested enable failures and cleanup on disable. |
| World APIs | Player, world, block, entity and inventory adapters backed by Rust state. Preserve event ordering, cancellation and visible client results. |
| Threading | Preserve synchronous Bukkit callback semantics. Parallel region APIs require explicit ownership and compatible plugins; arbitrary Bukkit calls cannot safely run on arbitrary worker threads. |
| Compatibility | Exercise independent plugins and publish their tested versions and scenarios. NMS/CraftBukkit/reflection compatibility is a separate contract. |
| Profiling | Integrate the original spark module and verify real Rust worker stacks, JVM plugin stacks and accurate memory attribution. A `/tps` command alone is not spark. |

Paper's API is not a runnable plugin server: its `JavaPluginLoader.loadPlugin()` rejects loading. `JavaPlugin` requires a configured plugin classloader and service implementations for `InternalAPIBridge` and `PluginLoader`. Returning successful no-ops or fabricated values for unimplemented APIs would conceal compatibility failures; unsupported calls must be explicit until their semantics exist. [Paper source](https://github.com/PaperMC/Paper/tree/35a2d155771786a370831ad2ad3c8bd45dc0bb1a/paper-api/src/main/java), [API setup](https://docs.papermc.io/paper/dev/project-setup/), [Java requirements](https://docs.papermc.io/paper/getting-started/).

Paper's synchronous scheduler and Folia's region ownership differ. Multithreaded networking and chunk generation are compatible with a serialized Java callback lane; running all existing plugins concurrently is not a compatible default. [Paper scheduler](https://docs.papermc.io/paper/dev/scheduler/), [Folia compatibility](https://github.com/PaperMC/Folia/blob/ver/26.3.x/README.md).

The original spark `PaperSparkModule` requires a Bukkit server adapter, scheduler, class lookup, lifecycle and tick callbacks. The Java profiler backend cannot establish Rust CPU usage. Its async-profiler backend supports native stacks on Linux/macOS; Windows Java fallback does not provide equivalent native coverage. A separate JVM process also cannot profile another process's Rust stacks by treating them as JVM stacks. Native RSS/allocation metrics must not be replaced with JVM heap measurements. [spark integration API](https://github.com/lucko/spark/blob/7ffae3d7ceb69df95480fd03e3f89d979c780fd8/spark-paper/src/main/java/me/lucko/spark/paper/api/PaperSparkModule.java), [spark engines](https://spark.lucko.me/docs/misc/Using-async-profiler), [async-profiler](https://github.com/async-profiler/async-profiler).

## Русский

Цель — **Minecraft 26.3**, Paper API 26.3 и **Java 25**. JAR сейчас является только запускатором Rust-файла. Он не загружает Java-плагины, не реализует Paper API и не содержит встроенный spark. Директория `plugins/java-plugins/` предназначена для будущей поддержки Java-плагинов сервера, `plugins/rust-plugins/` — для Rust-плагинов сервера. Клиентские моды к этому механизму не относятся.

Последовательность работ: JVM-адаптер → настоящий загрузчик и жизненный цикл → конфигурация/команды/права/scheduler → API мира и события → проверка независимых плагинов → настоящий spark. Каждая строка таблицы выше задаёт отдельный проверяемый этап. Игровая логика и данные остаются в Rust. Библиотеки размещаются в `libraries/`, кэш — в `.cache/`, конфигурация плагинов — в `config/`. Настройки ядра — `server.settings`, стандартные параметры сервера — `server.properties`.

Один вызов `onEnable()` не подтверждает совместимость Paper. Для неё нужны корректные изменения мира, события с отменой, порядок scheduler, обработка ошибок и очистка при отключении. NMS, CraftBukkit и reflection проверяются отдельно. Нереализованные методы не должны возвращать выдуманный успех.

Обычные Paper-плагины предполагают синхронный контекст. Многопоточная сеть, генерация и подготовка чанков допускают сохранение этого контракта; произвольный параллельный запуск Bukkit-обработчиков — нет. Для регионального исполнения необходимы отдельные гарантии владения данными и совместимость плагинов.

Оригинальный spark требует серверного адаптера и реальных метрик. Java backend не измеряет стеки Rust. Для Linux/macOS возможен native backend async-profiler, который нужно подтвердить профилем настоящей нагрузки в Rust worker. Windows fallback не даёт равнозначного профиля Rust. Метрики JVM heap нельзя выдавать за потребление памяти всего Rust-сервера. Команда `/tps` не заменяет эту интеграцию.
