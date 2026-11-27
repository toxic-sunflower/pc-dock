# Разработка

**О чём этот документ.** Для тех, кто дорабатывает PC Dock: как устроен репозиторий, как собрать и протестировать, как выпустить релиз и как добавить свой транспорт. Если вы только
пользуетесь продуктом, этот документ вам не нужен.

**Быстро.** `cd sdk && cargo build --release` собирает программы, `cargo test --workspace` запускает тесты (телефоны в них заменены фейками, поэтому физический телефон не нужен).
Перед коммитом: `cargo fmt` и `cargo clippy --workspace --all-targets -- -D warnings`.

## Структура репозитория (Rust-часть)

```
sdk/
  pcdock-core/     ядро (протокол, сессия, HID, ввод, интерфейсы транспорта)
  pcdock-usb/      AOA по USB (libusb), pcdock-probe
  pcdock-adb/      ADB: клиент сервера ADB, запуск серверного процесса
  pcdock-serial/   serial-труба
  pcdock-farm/     Farm, FarmConfig, Registry; examples/minimal.rs
  pcdock-net/      общая основа демонов: носители, ящик клиента, пароль
  pcdock-client/   клиент протокола демонов для Rust
  pcdock-autod/    демон автоматизации: Lua, прогоны, отчёты
  pcdock-check/    самопроверка (Windows, macOS, Linux)
  pcdock-serve/    протокол фермы (поверх pcdock-net), файлы
  pcdockd/         демон фермы
  docs/            эта документация
demos/             демо-клиенты протокола: web (WebSocket) и gui (pygame)
.github/workflows/ CI и релиз
devices/android/   протокол (PROTOCOL.md), решения (DECISIONS.md), приложение и сервер для телефона, сценарий проверки
```

## Сборка и проверка

```bash
cd sdk
cargo build                       # всё
cargo test                        # все тесты (около 170)
cargo clippy --all-targets        # без предупреждений
cargo fmt --check
```

Тесты `pcdock-usb`, которым нужен libusb (`Dock::new`), пропускаются, если контекст USB недоступен (CI, контейнер). Тесты фермы и
демона работают на фейковых каналах и не требуют телефонов. Проверка на телефонах: [`TESTING.md`](../../devices/android/TESTING.md).

Часть приложения и сервера для телефона (Kotlin): `cd devices/android/app-screencast && ./gradlew :app:assembleDebug
:server-adb:assembleDebug :lib-protocol:test`.

## Принципы

- Ядро (`pcdock-core`) не делает ввод-вывод, не читает часы и не содержит `unsafe` (`#![forbid(unsafe_code)]`). Время приходит параметром.
- Решения принимает ферма, а не транспорт и не демон. Транспорт сообщает факты и выполняет байты.
- Демон тонкий: никакой логики про телефоны.
- `unsafe` в проекте не используется (в ядре запрещён).
- Решения и их причины записываются в `devices/android/DECISIONS.md`.

## Как добавить транспорт

1. Реализуйте `PhoneLink` (подключить, прочитать и отправить байты, выполнить `InputCmd`, отпустить). Внутри держите
   `Session` из `pcdock-core`: ей отдаёте входящие байты, забираете то, что надо отправить (`Transmit::Stream` и
   `Transmit::Control`), и события. Образец: `StreamLink` в `pcdock-serial`.
2. Реализуйте `Discovery`, выдающий `Sighting` (путь, адрес, серийник, помеха). Если есть события (hotplug), верните `true` из
   `changed()`.
3. Добавьте вариант `Transport` в `pcdock-core::link` и ветку в фабрике `pcdock-farm::config` (или своя `LinkFactory`).
4. Ферма сама сделает слияние по серийнику, запасные переходы, состояния и потерю. Проверьте на фейках, как в тестах
   `pcdock-farm/src/runtime.rs`.

## Тесты фермы

`runtime.rs` содержит `FakeLink`, `FakeScan`, `FakeFactory` и тесты сценариев: появление, два пути, переключение, потеря и
возврат, закрепление, исключения, состояния с причинами, запасной переход, предпочтение, hotplug. Добавляйте сценарии
туда же, на фейках.

## Релиз

Версия задаётся в `sdk/Cargo.toml` (`[workspace.package]`). Тег `vX.Y.Z` запускает `.github/workflows/release.yml`: бинарники для Linux, Windows и macOS, APK
приложения и сервера (подпись из секретов репозитория `ANDROID_KEYSTORE_B64`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`), `SHA256SUMS`.
Ключ подписи в репозиторий не кладётся.
