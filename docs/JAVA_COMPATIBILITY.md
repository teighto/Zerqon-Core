# Java plugins and spark

## English

Zerqon runs Java plugins with Java 25. Put their JAR files in `plugins/java-plugins/`. Rust plugins go in `plugins/rust-plugins/`. The server itself runs as `zerqon` or `zerqon.exe`.

Plugins written for Zerqon already work. Support for existing Paper plugins is being added. The current adapter passes tests for loading plugins, reading their settings, running commands and scheduled tasks, and handling events. Access to the world, players and inventories still needs work, so an arbitrary Paper plugin may not run yet.

The built-in Rust profiler captures CPU stacks on Linux and saves reports that open in spark. Windows sampling is not implemented yet. `/tps` shows tick timing.

[Write a Java plugin](JAVA_PLUGINS.md) · [Write a Rust plugin](RUST_PLUGINS.md)

## Русский

Для Java-плагинов нужна Java 25. Их JAR-файлы кладутся в `plugins/java-plugins/`, Rust-плагины в `plugins/rust-plugins/`. Сам сервер запускается через `zerqon` или `zerqon.exe`.

Плагины для собственного API Zerqon уже работают. Поддержка готовых Paper-плагинов добавляется отдельно. Проверены загрузка, настройки плагина, команды, отложенные задачи и обработка событий. Доступ к миру, игрокам и инвентарям ещё дорабатывается, поэтому любой скачанный Paper-плагин пока запускаться не обязан.

Встроенный Rust-профилировщик записывает CPU-профиль на Linux. Его отчёты открываются в spark. Сбор CPU-профиля на Windows пока не реализован. `/tps` показывает время работы тиков.

[Написать Java-плагин](JAVA_PLUGINS.md) · [Написать Rust-плагин](RUST_PLUGINS.md)
