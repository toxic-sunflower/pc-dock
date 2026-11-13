# Протокол демонов (версия 1)

Это единственный внешний интерфейс продукта. Его говорят оба демона: `pcdockd` (ферма телефонов) и `pcdock-autod` (автоматизация).
Общее (конверт, версия, пароль, номера запросов) описано в первой части, сообщения каждого демона в следующих.

Для телефонов и ПК есть ещё один протокол: между ПК и самим телефоном ([`PROTOCOL.md`](../../devices/android/PROTOCOL.md)). Клиентам демонов он
не нужен.

## 1. Общее

### Носители

| Носитель | Параметр | Конверт |
|---|---|---|
| TCP | `--tcp` | потоковый (ниже) |
| Локальная труба | `--local` | потоковый |
| WebSocket | `--ws` | текстовый фрейм JSON, бинарный фрейм `[тип][нагрузка]` |

Локальная труба: Linux абстрактный unix-сокет `@имя`; macOS сокет `/tmp/имя`; Windows именованный канал `\\.\pipe\имя`.

### Потоковый конверт

`u32` длина (little-endian, считая байт типа и тело), байт типа, тело. Типы:

| Тип | Содержимое |
|---|---|
| `0` | JSON (UTF-8) |
| `1` | видеокадр (`pcdockd`) |
| `2` | звук (`pcdockd`) |
| `3` | данные файла (`pcdockd`, в обе стороны) |
| `4` | вложение прогона (`pcdock-autod`) |

Максимальная длина пакета 32 МБ.

### Приветствие и версия

Сразу после подключения (до пароля) сервер шлёт `{"ev":"Hello","protocol":1,"server":"pcdockd/0.1.0-alpha.3"}`; у `pcdock-autod` добавляется
`"role":"autod"`. Клиент проверяет `protocol`. Версия растёт при несовместимых изменениях.

### Пароль

Если демон запущен с `--token`, первой командой должна быть `{"op":"Auth","token":"…"}`; ответ `{"ev":"Authed"}`. До этого любые другие команды
получают `Error`, события не приходят. На неверный токен ответ задерживается на 0,5 с.

### Команды и ответы

Команда это JSON-объект с полем `op` и необязательным числовым `id`. Если `id` есть, ответ на команду несёт то же `id`:

```
→ {"op":"List","id":41}
← {"ev":"Phones","phones":[1],"details":[…],"id":41}
→ {"op":"Wake","phone":1,"id":42}
← {"ev":"Ok","id":42}                    команда без собственного ответа
→ {"op":"Wake","phone":99,"id":43}
← {"ev":"Error","detail":"нет трубки 99","id":43}
```

Без `id` ответа `Ok` нет, ошибки приходят без `id`. События (сообщения без `id`) приходят независимо от команд. Поле `id` занято
под номер запроса: идентификаторы вещей (например, сценария) в сообщениях называются иначе (`script`, `run`, `phone`, `transfer`).

### Медленные клиенты

У каждого клиента отдельный исходящий ящик: события JSON выдаются все и по порядку; звук идёт очередью ограниченной длины (лишнее отбрасывается);
видео хранится одним слотом (быстрый клиент получает каждый кадр, медленный самый свежий; пропуск виден по `seq`); данные файлов идут с ожиданием.
Клиент, у которого события JSON копятся без чтения (более 4096 штук или 2 МБ), отключается сервером.

## 2. `pcdockd`: ферма

### Команды

| `op` | Поля | Ответ |
|---|---|---|
| `List` | | `Phones {phones, details}`; `details`: для каждого `{phone, state, transport, address, serial, name, display}` |
| `Watch` | `phone`, `fps?`, `on_change?`, `audio?` | `Watched`; затем последний кадр из кэша. Повторный `Watch` меняет условия |
| `Unwatch` | `phone` | `Watched {on:false}` |
| `Snapshot` | `phone` | `Ok` (если был `id`) и один видеопакет только вам; нет кадра: `Error` |
| `TouchEx` | `phone, cid, phase, x, y, width?, height?, pressure?` | `Ok` (если был `id`) |
| `CursorMoveAbs` / `CursorMoveRel` / `CursorResync` | `phone, x, y` / `phone, dx, dy` / `phone` | |
| `MouseButton` | `phone, button, down` | `button`: 0 левая, 1 правая, 2 средняя |
| `Scroll` / `ScrollH` | `phone, ticks` | |
| `Key` | `phone, usage, down` | `usage`: код USB HID |
| `DeviceEnable` | `phone, kind, on` | `kind`: 0 касание, 1 мышь, 2 клавиатура |
| `ReleaseAll`, `RequestFrame`, `Wake` | `phone` | |
| `SetTransport` | `phone`, `transport` (`usb`, `adb`, `serial`) | перевести телефон на другой путь; `Ok` значит «принято», итог придёт событием `Appeared` (успех) или `Warning` (путь недоступен) |
| `SetStream` | `phone, video, audio` | |
| `FilePut` | `phone, dest, size, media?` | `FileReady {transfer}`, затем данные клиента (тип 3), итог `FileDone`/`FileFailed` |
| `FileGet` | `phone, src` | `FileInfo {transfer, size}`, данные сервера (тип 3), итог `FileDone`/`FileFailed` |
| `FileList` | `phone, path` | `Files {phone, path, names}` |
| `FileDelete` | `phone, path` | `Ok` |
| `FileCancel` | `transfer` | `Ok` |
| `Pin` | `spec` (`usb:1-2.3`, `usb:СЕРИЙНИК`, `adb:СЕРИЙНИК`, `serial:ПУТЬ`) | закрепить телефон на ходу; `Ok`, затем `Appeared` при ближайшем обнаружении. Для `pcdockd --manual`: приложение подключает телефоны по одному |
| `Unpin` | `spec` | снять закрепление: телефон отключается (USB возвращается в обычный режим), `Lost` |
| `AppEvents` | `phone, on` | получать `Status` приложения телефона (передний план 16/17, ответы) без `Watch`; `Ok` |
| `LaunchApp` | `phone, request, package` | открыть приложение; ответ событием `Status` 18 `"<request> <результат>"` |
| `ListApps` | `phone, request` | приложения с главным экраном; `Status` 19 `"<request> <JSON>"` |
| `UiDump` | `phone, request` | элементы экрана (uiautomator XML); `Status` 20 частями `"<request> <i>/<n> <часть>"` |
| `ExitInfo` | `phone, request, since_ms, package` | причины завершения приложения; `Status` 22 `"<request> <JSON>"` |
| `IpCheck` | `phone, request, url` | HTTPS GET по сети телефона (`https://` только); `Status` 23 `"<request> <JSON>"` |
| `HttpLocal` | `phone, request, url`, `method?` (`GET` по умолчанию, `POST`), `headers?` (`{"имя":"значение"}`), `body?` (только `POST`, ≤ 64 КБ), `timeout_ms?` (0/нет — 10 с, не больше 30 с) | HTTP-запрос к устройству локальной Wi-Fi-сети телефона (админка роутера, в Wi-Fi которого телефон, а компьютер — нет); `url` — `http(s)://` с IPv4 частной сети (10/8, 172.16/12, 192.168/16, 169.254/16). `Status` 24 `"<request> <JSON>"`: `{"ok":true,"code":401,"body":"…","headers":{"set-cookie":"…"},"ms":35}` (любой код HTTP — успех) или `{"ok":false,"error":"no_wifi","ms":2}` |
| `TypeText` | `phone, request, text` (не пустой, ≤ 16 КБ UTF-8) | ввести текст любого языка (кириллица, эмодзи, переводы строк) в поле с курсором: телефон вставляет его службой специальных возможностей или кладёт в буфер обмена. `Status` 25 `"<request> <JSON>"`: `{"ok":true,"method":"paste"\|"set_text","ms":40}` — текст в поле; `{"ok":true,"method":"clipboard","reason":"no_service","ms":3}` — текст только в буфере обмена, клиент жмёт Ctrl+V сам (`Key` 0xE0, 0x19); `{"ok":false,"error":"bad_request"}` |

Запросы к приложению (возможности 7–13, `devices/android/PROTOCOL.md`): ответ `Status` приходит запросившему клиенту и без
`Watch`. Если телефон не умеет или запрос неверен, ответ всё равно приходит: `"<request> error <причина>"` (`unsupported`,
`not_active`, `invalid_package`, `invalid_url`, `invalid_request`; у `UiDump` — `"<request> 1/1 error <причина>"`). Сообщения приложения без
запроса — передний план (`Status` 16/17), «экран изменился» (21) — получают подписчики и те, кто уже спрашивал приложение.

Координаты, размер пятна и сила касания доли `0..1` отображаемой картинки; `phase`: 0 down, 1 move, 2 up, 3 cancel.

### События

| `ev` | Поля | Кому |
|---|---|---|
| `Appeared` | `phone, transport, address, serial` | всем |
| `State` | `phone, state, detail` | всем (`appeared`, `connecting`, `ready`, `needs_permission`, `needs_app`, `needs_access`, `needs_auth`, `lost`) |
| `Lost` | `phone` | всем |
| `Connected` | `phone, version, caps, name` | подписчикам телефона |
| `Display` | `phone, stream_w, stream_h, screen_w, screen_h, dpi, rotation` | подписчикам |
| `AudioConfig` | `phone, codec, rate, channels` | подписчикам |
| `Status`, `PeerSilent`, `Malformed`, `Disconnected`, `LinkDown`, `Warning` | `phone` и сведения | подписчикам |
| `FileProgress` | `transfer, bytes` | владельцу передачи |

### Подписка на кадры

`Watch {phone, fps?, on_change?, audio?}`: `fps` не чаще стольких кадров в секунду (без него все кадры); `on_change` только кадры, отличающиеся от
предыдущего; `audio` присылать и звук. Видео на телефоне идёт по спросу: нет подписчиков, видео выключено; иначе телефон снимает с частотой
самого требовательного. Подписки снимаются при отключении клиента. Разрешение кадров всегда оригинальное: демон их не масштабирует.

### Видеопакет (тип 1)

| Смещение | Размер | Поле |
|---|---|---|
| 0 | 4 | номер телефона (u32 LE) |
| 4 | 8 | порядковый номер кадра (u64 LE) |
| 12 | 1 | кодек (1 = JPEG) |
| 13 | 1 | ключевой кадр (0/1) |
| 14 | 8 | метка времени, мкс (u64 LE) |
| 22 | … | JPEG |

### Звуковой пакет (тип 2)

`u32 LE` номер телефона, `u64 LE` метка времени (мкс), затем PCM 16 бит little-endian; частота и каналы из `AudioConfig`.

### Файлы (тип 3)

Включаются `pcdockd --files`; работают по ADB (для телефона должен быть известен серийник). Пути только на телефоне, в разрешённых папках
(по умолчанию `/sdcard`, `/storage/emulated/0`, `/data/local/tmp`), абсолютные, без `..`, пробелов и спецсимволов. Путь на ПК от клиента не принимается:
данные идут потоком через демон.

- Запись: `FilePut` → `FileReady {transfer}` → клиент шлёт ровно `size` байт пакетами типа 3 вида `[transfer u32 LE][данные]` → `FileDone {transfer, size}`.
  Лишние байты, обрыв клиента, отмена или несовпадение размера дают `FileFailed {transfer, detail}`, неполный файл не остаётся.
- Чтение: `FileGet` → `FileInfo {transfer, size}` → сервер шлёт пакеты типа 3 → `FileDone`. `FileDone` приходит строго после последнего пакета данных.
- `media: true` просит галерею заметить новый файл (на новых версиях Android действует не везде).

## 3. `pcdock-autod`: автоматизация

Клиент `pcdockd`, который хранит сценарии на Lua и запускает их как автотесты. События прогона идут в прямом эфире с повтором истории.

### Команды

| `op` | Поля | Ответ |
|---|---|---|
| `Phones` | | `Phones {phones, details}` (телефоны фермы) |
| `ScriptList` | | `Scripts {scripts:[{script, name, created, updated, size}]}` |
| `ScriptGet` | `script` | `Script {script, name, source, updated}` |
| `ScriptPut` | `script?, name, source` | `ScriptSaved {script, name}`; без `script` создаётся новый |
| `ScriptRename` | `script, name` | `Ok` |
| `ScriptDelete` | `script` | `Ok` |
| `Run` | `script, phones?, args?, tests?, timeout?, follow?` | `RunStarted {run, script, name, phones}`; затем события прогона (если не `follow:false`) |
| `Stop` | `run` | `Ok` |
| `Runs` | | `Runs {runs:[{run, script, name, phones, started, finished, status}]}` |
| `RunInfo` | `run` | `Run {run, summary, report, artifacts}` |
| `Subscribe` | `run, from?` | `Ok`; затем события с номера `from` (история, потом живой поток) |
| `Artifacts` | `run` | `Artifacts {run, names}` |
| `Artifact` | `run, name` | `Artifact {run, name, size}` и пакет типа 4: `[длина имени u8][имя][содержимое]` |

`Run`: `phones` телефоны, выделяемые прогону (по умолчанию все готовые); телефон занимается эксклюзивно, пока прогон идёт; `args` параметры, доступные
сценарию как `args`; `tests` выполнить только тесты с этими именами; `timeout` срок прогона в секундах.

### События

Всем клиентам: `RunStarted {run, script, name, phones}` и `RunEnded {run, status}`. Подписчикам прогона: `RunEvent`.

`RunEvent` общие поля: `run`, `seq` (сквозной номер с нуля), `ts` (мс), `kind`, остальное зависит от вида:

| `kind` | Поля |
|---|---|
| `run_start` | `script, phones, args` |
| `test_start` | `test` |
| `step` | `test, text` |
| `log` | `test, level (debug/info/warn/error), text` |
| `assert` | `test, mode (assert/expect), op, ok, message, expected, actual` |
| `artifact` | `name, kind, phone, seq, reason (assert/script)` |
| `test_end` | `test, status (passed/failed/error/stopped), duration_ms, asserts_passed, asserts_failed, error` |
| `run_end` | `status, duration_ms, passed, failed, errors, tests` |

Статус прогона: `passed`, `failed` (проверка не прошла), `error` (ошибка в сценарии или вышло время), `stopped`.

### Хранение

Папка данных (`--data`): `scripts/<id>.lua` и `scripts/index.json` (id → читаемое имя, даты, размер), `runs/<run>/events.jsonl` (все события),
`runs/<run>/report.json` (итоговый отчёт) и вложения (кадры). Файлы и идентификаторы выдаёт демон; пути от клиента не принимаются.
