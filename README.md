# PC Dock

**PC Dock управляет Android-телефонами с компьютера.** Подключили телефон кабелем, запустили программу, и можно смотреть на его экран, нажимать, открывать приложения и писать
сценарии, которые делают это сами: автотесты, ночные проверки, рутина на десятке телефонов. Интерфейс не нужен, всё работает из скрипта.

Не знаете, с чего начать: [что это и зачем](sdk/docs/what-is-pcdock.md), потом [быстрый старт](sdk/docs/getting-started.md) (10–15 минут) и [рецепты](sdk/docs/recipes.md).

Пример: сценарий на Lua, который открывает «Настройки» и проверяет, что на экране есть пункт «Wi-Fi»:

```lua
test("Настройки открываются", function()
  local p = phones()[1]
  p:launch("com.android.settings")
  assert.truthy(p:wait_for("Wi-Fi", 8), "пункт Wi-Fi на экране")
end)
```

## Подробнее

Демоны для управления Android-телефонами с ПК: экран (картинка и звук), ввод (касания с площадью и силой, мышь, клавиатура), файлы и
автоматизация сценариями на Lua как автотесты. Телефонов может быть много; подключение, отключение и выбор пути обрабатываются автоматически.

**Статус: альфа (0.1.0-alpha.3).** Основа работает на реальных телефонах (Linux, Android 15); Windows и macOS ждут проверки. См. «Что проверено» в
[`sdk/docs/README.md`](sdk/docs/README.md) и ограничения в [`sdk/docs/operations.md`](sdk/docs/operations.md).

## Что внутри

```
pcdockd        демон фермы: находит телефоны, держит подключения, раздаёт кадры, принимает ввод и файлы
pcdock-autod   демон автоматизации: сценарии на Lua, проверки, логи, прямой эфир, отчёты JSON
pcdock-check   самопроверка на вашей машине (Linux, macOS, Windows)
```

Единственный внешний интерфейс это протокол демонов (JSON и бинарные пакеты; TCP, WebSocket, локальная труба): [`sdk/docs/protocol.md`](sdk/docs/protocol.md).
Телефон подключается тремя путями: USB-аксессуар (AOA, нужно приложение), ADB (ничего не устанавливается, диалога записи экрана нет), serial.

## Установка

Сайт: **https://pc-dock.prozorov.me** (установка, быстрый старт, документация).

```bash
curl -fsSL https://pc-dock.prozorov.me/install.sh | sh     # Linux, macOS
irm https://pc-dock.prozorov.me/install.ps1 | iex           # Windows (PowerShell)
```

Установщик скачивает последний релиз, проверяет `SHA256SUMS`, ставит `pcdockd`, `pcdock-autod`, `pcdock-check` в `~/.pcdock/bin` и добавляет каталог в PATH
(на Linux и macOS подскажет строку для `~/.profile`). Версию можно задать: `sh install.sh v0.1.0-alpha.3`.

Через Rust: `cargo install --locked --git https://github.com/toxic-sunflower/pc-dock pcdockd pcdock-autod pcdock-check pcdock-usb`.
Все способы, обновление и удаление: [sdk/docs/install.md](sdk/docs/install.md). Если сайт недоступен, скрипт лежит в репозитории: `https://raw.githubusercontent.com/toxic-sunflower/pc-dock/main/install.sh`.

## Быстрый старт

```bash
cd sdk && cargo build --release                    # нужен Rust и libusb (Linux: libusb-1.0-0-dev)
cd ../devices/android/app-screencast && ./gradlew :server-adb:assembleDebug   # сервер для ADB-режима
```

Включите на телефоне отладку по USB, подключите кабель и проверьте окружение:

```bash
sdk/target/release/pcdock-check --server-apk devices/android/app-screencast/server-adb/build/outputs/apk/debug/server-adb-debug.apk --input
```

Запуск:

```bash
sdk/target/release/pcdockd --auto --server-apk .../server-adb-debug.apk       # ферма (TCP 9001, WebSocket 9002)
sdk/target/release/pcdock-autod                                                # автоматизация (TCP 9101, WebSocket 9102)
```

Демо-клиенты протокола: `demos/web/index.html` (один файл, откройте в браузере) и `demos/gui` (pygame: `uv run app.py`).

## Документация

[`sdk/docs/`](sdk/docs/README.md): с чего начать (по ролям), быстрый старт, рецепты, руководство по эксплуатации, протокол, демоны, сценарии Lua, пути подключения, архитектура, разработка.
Решения и причины: [`devices/android/DECISIONS.md`](devices/android/DECISIONS.md). План: [`devices/android/ROADMAP.md`](devices/android/ROADMAP.md).

## Состав репозитория

```
sdk/                     Rust: ядро, транспорты, ферма, демоны, клиент, самопроверка (+ docs/)
devices/android/         приложение (AOA) и сервер (ADB) для телефона на Kotlin, протокол ПК-телефон, решения, сценарий проверки
demos/                   демо-клиенты протокола (web, gui)
.github/workflows/       CI (Linux, Windows, macOS, Android) и релиз по тегу
```

## Лицензия

`MIT OR Apache-2.0` (файлы `LICENSE-MIT`, `LICENSE-APACHE`).
