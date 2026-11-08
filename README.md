# Zerqon

[English](#english) · [Русский](#russian) · [Настройки / Configuration](docs/CONFIGURATION.md)

![Zerqon server architecture](docs/images/architecture.png)

<a id="english"></a>
## English

Zerqon is a Minecraft 26.3 server written in Rust. The focus is on speed, reliability and control over memory use. Plugins can be written in Java or Rust.

The server keeps the familiar folder layout: worlds, logs, configuration files and a plugins folder. Java and Rust plugins can run on the same server, while players connect with their usual client.

### Paper and Zerqon

| | Paper | Zerqon |
|---|---|---|
| Server engine | Java, running on the JVM | Native Rust executable |
| Plugins | Bukkit, Spigot and Paper Java plugins | Own Java and Rust APIs; Paper support in development |
| Java runtime | Required for the server | Used for Java plugins and initial game-data preparation |
| Worlds | Vanilla world storage | Own storage; world import is being developed |
| Profiling | Bundled spark | Built-in Rust CPU profiler on Linux; spark reports |
| Availability | Published server releases | 0.0.1 is not released yet |

Paper details: [PaperMC documentation](https://docs.papermc.io/paper/). A speed or RAM comparison between the two servers needs the same world and workload; that comparison has not been run yet.

### Performance

Reading and decoding block data from 256 chunks of a normal vanilla world, seed `20261108`. Each run reads every chunk 250 times after warmup: 64,000 chunk reads in total.

![Ordinary-world block storage measurements](docs/images/performance.png)

Ryzen 9 9900X, 32 GB RAM, Samsung 970 EVO Plus; WSL2 with ext4, Rust 1.97 release build for Linux musl. Measured on 8 November 2026, with three fresh processes for each worker setting.

| Workers | Chunk reads/s | Time for 64,000 reads | CPU | Peak RSS |
|---:|---:|---:|---:|---:|
| 1 | 10,039 | 6.375 s | 105.4% | 11.36 MiB |
| 4 | 11,807 | 5.421 s | 242.0% | 12.12 MiB |
| 8 | 17,866 | 3.582 s | 446.7% | 12.24 MiB |

Speed, time and CPU are medians; RSS is the highest observed value. The error bars show the slowest and fastest runs. CPU at 100% means one logical core. RAM describes this storage test; a game server also needs memory for players, plugins and other services.

Reusing chunk buffers improved throughput by 7–35% in this workload. The timer excludes world import, generation, lighting, entities and network delivery. Normal terrain generation in Rust and pregeneration are not ready yet, so there are no speed figures for them.

[Measurements and run details](docs/benchmarks/ordinary-chunks.json) · [Repeat the test](https://github.com/teighto/Zerqon-Core/tree/main/crates/zerqon-world#ordinary-terrain-block-loading)

### Start the server

Run the compiled file. On Linux:

```sh
chmod +x zerqon
./zerqon
```

On Windows, run `zerqon.exe`.

The first start creates the configuration files. Read the [Minecraft EULA](https://aka.ms/MinecraftEULA), set `eula=true` if you agree, then start again. Zerqon prepares the game data automatically and downloads Java 25 if needed. Downloads normally stay in `.cache/`. On Windows, Java uses a [separate user cache](docs/CONFIGURATION.md) when the server path contains non-ASCII characters. Python is not needed to run the server.

Set the port in `server.properties` and the thread counts in `server.settings`. Type `stop` in the console to shut down.

To build and run from source with Rust 1.97:

```sh
cargo run --release
```

`cargo build --release` produces `target/release/zerqon` (`zerqon.exe` on Windows). Java plugin development has its own [build instructions](docs/JAVA_PLUGINS.md).

### Settings and plugins

![Java and Rust plugin loading](docs/images/plugins.png)

```text
server.properties
server.settings
eula.txt
plugins/
  rust-plugins/
  java-plugins/
libraries/
.cache/
logs/
world/
```

`server.properties` contains the address, port, MOTD and Minecraft settings. `server.settings` contains thread counts, cache limits and plugin settings. Start with the defaults, then adjust the values for your server.

Put Rust `.wasm` plugins in `plugins/rust-plugins/` and Java `.jar` plugins in `plugins/java-plugins/`. Java plugins can use Zerqon's own API; some Paper features also work through the experimental bridge. Check the [compatibility list](docs/JAVA_COMPATIBILITY.md) before moving a plugin. Players do not need to install server plugins.

[Configuration](docs/CONFIGURATION.md) · [Rust plugin development](docs/RUST_PLUGINS.md) · [Java plugin development](docs/JAVA_PLUGINS.md) · [Paper compatibility](docs/JAVA_COMPATIBILITY.md)

<a id="russian"></a>
## Русский

Zerqon, сервер Minecraft на версии 26.3, написанный на Rust. Основной упор на скорость, надёжность и контроль расхода памяти. Плагины можно писать на Java и Rust.

Папки остаются привычными: миры, логи, настройки и плагины. На одном сервере можно использовать Java- и Rust-плагины. Игроки подключаются обычным клиентом.

### Paper и Zerqon

| | Paper | Zerqon |
|---|---|---|
| Движок сервера | Java, работает в JVM | Нативный исполняемый файл Rust |
| Плагины | Java-плагины Bukkit, Spigot и Paper | Собственные API Java и Rust; поддержка Paper в разработке |
| Java | Нужна для работы сервера | Нужна Java-плагинам и для первоначальной подготовки игровых данных |
| Миры | Формат хранения vanilla | Собственное хранение; импорт миров разрабатывается |
| Профилирование | Встроенный spark | Rust-профилировщик CPU на Linux, отчёты spark |
| Выпуск | Есть опубликованные релизы | 0.0.1 пока не выпущена |

Сведения о Paper: [документация PaperMC](https://docs.papermc.io/paper/). Сравнение скорости и памяти требует одинакового мира и нагрузки. Такого замера пока нет.

### Производительность

Чтение и декодирование блоков из 256 чанков обычного мира vanilla, seed `20261108`. После прогрева каждый чанк читается 250 раз: всего 64 000 чтений за прогон.

![Замеры чтения блоков обычного мира](docs/images/performance.png)

Ryzen 9 9900X, 32 ГБ RAM, Samsung 970 EVO Plus; WSL2 с ext4, Rust 1.97, release-сборка Linux musl. Замеры от 8 ноября 2026 года, по три отдельных запуска для каждого числа потоков.

| Потоки | Чтений чанков/с | Время 64 000 чтений | CPU | Пик RSS |
|---:|---:|---:|---:|---:|
| 1 | 10 039 | 6.375 с | 105.4% | 11.36 МиБ |
| 4 | 11 807 | 5.421 с | 242.0% | 12.12 МиБ |
| 8 | 17 866 | 3.582 с | 446.7% | 12.24 МиБ |

Для скорости, времени и CPU указана медиана; для RSS — наибольшее измеренное значение. Отметки на графике показывают самый медленный и самый быстрый прогоны. 100% CPU означает одно логическое ядро. Память указана для этого теста хранилища; игровому серверу дополнительно нужна память для игроков, плагинов и остальных служб.

После удаления лишней копии чанка чтение ускорилось на 7–35% в этом тесте. Импорт мира, генерация, освещение, сущности и отправка по сети в замер не входят. Обычная генерация на Rust и предгенерация пока не готовы, поэтому цифр их скорости ещё нет.

[Замеры и параметры прогонов](docs/benchmarks/ordinary-chunks.json) · [Как повторить тест](https://github.com/teighto/Zerqon-Core/tree/main/crates/zerqon-world#ordinary-terrain-block-loading)

### Запуск

На Linux запустите готовый файл:

```sh
chmod +x zerqon
./zerqon
```

На Windows запустите `zerqon.exe`.

Первый запуск создаёт настройки. Прочитайте [Minecraft EULA](https://aka.ms/MinecraftEULA), при согласии установите `eula=true` и запустите сервер ещё раз. Игровые данные и нужная Java 25 загрузятся автоматически. Обычно загрузки хранятся в `.cache/`. Если путь сервера содержит кириллицу или другие символы вне ASCII, на Windows Java скачивается в [отдельный кеш пользователя](docs/CONFIGURATION.md). Python для запуска не нужен.

Порт задаётся в `server.properties`, потоки в `server.settings`. Для остановки введите `stop` в консоли.

Из исходников с установленным Rust 1.97:

```sh
cargo run --release
```

После `cargo build --release` готовый файл лежит в `target/release/`. Для разработки Java-плагинов есть отдельная [инструкция](docs/JAVA_PLUGINS.md).

### Настройки и плагины

Адрес, порт, MOTD и настройки Minecraft находятся в `server.properties`. Потоки, кеш и настройки плагинов находятся в `server.settings`. Все значения по умолчанию создаются при первом запуске.

Rust-плагины `.wasm` кладутся в `plugins/rust-plugins/`, Java-плагины `.jar` в `plugins/java-plugins/`. Для Java есть собственный API Zerqon и экспериментальная поддержка части Paper. Перед переносом плагина проверьте [список совместимости](docs/JAVA_COMPATIBILITY.md). Игрокам устанавливать серверные плагины не нужно.

[Настройки](docs/CONFIGURATION.md) · [Разработка на Rust](docs/RUST_PLUGINS.md) · [Разработка на Java](docs/JAVA_PLUGINS.md) · [Совместимость с Paper](docs/JAVA_COMPATIBILITY.md)

## License / Лицензия

[GPL-3.0](LICENSE). Автор: [teighto](https://github.com/teighto).
