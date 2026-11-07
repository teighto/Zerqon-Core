# Zerqon

[English](#english) · [Русский](#russian) · [Настройки / Configuration](docs/CONFIGURATION.md)

![Zerqon server architecture](docs/images/architecture.png)

<a id="english"></a>
## English

Zerqon is a Minecraft 26.3 server written in Rust by teighto. The project focuses on memory use and splitting server work across CPU cores. Plugins can be written in Java or Rust.

The server keeps the familiar folder layout: worlds, logs, configuration files and a plugins folder. Java and Rust plugins can run on the same server, while players connect with their usual client.

Version 0.0.1 is in development. Players can join, see each other and mobs, and keep their inventory between restarts. Java and Rust plugins run together. Existing worlds can be imported for testing, with their original spawn point. Survival, complete world transfers and full Paper support still need work.

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

![Chunk generation measurements](docs/images/performance.png)

Ryzen 9 9900X, Windows 11, Rust 1.97 release build. Median of three runs with 10,000 flat chunks on 7 November 2026.

| Test | Result |
|---|---:|
| Generate chunks on one thread | 22,787 chunks/s |
| Generate chunks with four workers | 64,288 chunks/s |
| Decode saved chunks | 115,524 chunks/s |

One flat chunk uses 2,920 bytes for its structure and section allocations; its saved file is 1,790 bytes. These figures exclude allocator overhead and total process memory. The test measures chunk processing, not gameplay or player capacity.

```sh
cargo run -p zerqon-world --example world_bench --release --locked -- 10000 4
```

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

Zerqon, сервер Minecraft 26.3 на Rust от teighto. Работа над ядром сосредоточена на расходе памяти и распределении нагрузки между ядрами процессора. Плагины можно писать на Java и Rust.

Папки остаются привычными: миры, логи, настройки и плагины. На одном сервере можно использовать Java- и Rust-плагины. Игроки подключаются обычным клиентом.

Версия 0.0.1 в разработке. Игроки видят друг друга и мобов, инвентарь сохраняется между запусками. Java- и Rust-плагины работают вместе. Готовый мир можно импортировать для проверки: исходная точка появления сохраняется. Выживание, полный перенос миров и совместимость со всем Paper API ещё дорабатываются.

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

Ryzen 9 9900X, Windows 11, Rust 1.97, release-сборка. Медиана трёх запусков по 10 000 плоских чанков, 7 ноября 2026 года.

| Тест | Результат |
|---|---:|
| Генерация в один поток | 22 787 чанков/с |
| Генерация в четыре потока | 64 288 чанков/с |
| Чтение сохранённых чанков | 115 524 чанков/с |

Структура плоского чанка и выделенная память секций занимают 2 920 байт, файл чанка на диске занимает 1 790 байт. Накладные расходы аллокатора и остальная память процесса сюда не входят. Это замер обработки чанков; по нему нельзя определить количество игроков на сервере.

```sh
cargo run -p zerqon-world --example world_bench --release --locked -- 10000 4
```

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
