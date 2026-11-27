# Протокол демонов (версия 1)

## Что это и как читать

Протокол это договорённость о том, **как ваша программа разговаривает с демоном**: что отправить, чтобы телефон нажал кнопку, и что демон ответит. Любая программа
на любом языке может быть клиентом: нужно открыть соединение и обмениваться сообщениями.

Устройство разговора очень простое:

1. Вы открываете соединение с демоном (TCP на `127.0.0.1:9001` для `pcdockd`, `9101` для `pcdock-autod`; либо WebSocket для браузера).
2. Демон сразу присылает приветствие `{"ev":"Hello", …}`: он сообщает версию протокола и своё имя.
3. Вы отправляете **команду**: JSON-объект, в котором поле `op` называет действие, а остальные поля его параметры.
4. Демон отвечает **событием**: JSON-объект, в котором поле `ev` называет, что это.

**Полный пример разговора** (настоящий, с реальным телефоном). Команда, ответ:

```
→ {"op": "List", "id": 1}
← {"ev": "Phones", "id": 1, "phones": [1],
   "details": [{"phone": 1, "state": "ready", "transport": "adb", "serial": "35022250", "name": "pcdock-server/1.0",
                "display": {"screen_w": 1080, "screen_h": 2412, "dpi": 480, "rotation": 0}}]}

→ {"op": "LaunchApp", "phone": 1, "package": "com.android.settings", "id": 2}
← {"ev": "DeviceResult", "id": 2, "phone": 1, "op": "LaunchApp", "data": {}}

→ {"op": "LaunchApp", "phone": 7, "package": "com.android.settings", "id": 3}
← {"ev": "Error", "id": 3, "detail": "нет трубки 7"}
```

Что здесь видно:

- Поле **`id`** необязательное число, которое вы придумываете сами. Демон повторяет его в ответе, так что при нескольких командах сразу вы знаете, какой ответ к какой команде.
- **`phone`** номер телефона из ответа `List`. Он нужен почти во всех командах.
- Если что-то не так, приходит **`{"ev":"Error","detail":"…"}`** с понятным текстом. Ошибка всегда говорит, в чём дело.
- Кроме ответов, демон шлёт **события без `id`**, когда что-то случилось само: телефон подключился (`Appeared`), сменил состояние (`State`), пропал (`Lost`).
  Клиент их можно игнорировать или показывать.

Короткий клиент для опытов: [`sdk/examples/pcdock.py`](../examples/pcdock.py). Пример: `python3 sdk/examples/pcdock.py '{"op":"List"}'`.

**Как устроен документ.** Часть 1 общее для обоих демонов (как упаковываются сообщения, пароль). Часть 2 команды `pcdockd`: телефоны, экран, ввод, приложения, файлы, запись.
Часть 3 команды `pcdock-autod`: сценарии, прогоны, цепочки, расписание. Дальше таблицы: для каждой команды поля и ответ. Таблицы это справочник, их не надо читать подряд: ищите нужную команду.

Для телефонов и ПК есть ещё один протокол: между ПК и самим телефоном ([`PROTOCOL.md`](../../devices/android/PROTOCOL.md)). Клиентам демонов он не нужен.

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

### Команды над телефоном

Команды `DeviceInfo`, `DeviceStats`, `AppInfo`, `ForceStop`, `ClearAppData`, `UninstallApp`, `InstallApk`, `Logcat`, `SetMobileData`,
`StayAwake`, `ScreenState`, `Button` отвечают событием `{"ev":"DeviceResult","phone":N,"op":"ИМЯ","data":{…}}` (с тем же `id`). Выполняются
по ADB в отдельном потоке (установка не задерживает остальных); у телефона должен быть известен серийник и доступен ADB, иначе ответ `Error`.
Демон запуска `pcdockd --no-device-ops` их отключает.

Элемент экрана (`UiFind`, `UiDump` …): `text`, `desc`, `id`, `class`, `package`, `kind`, `bounds` `[l,t,r,b]` в пикселях, `x`, `y` (центр), `w`, `h` в долях экрана
(готовы для касания), `clickable`, `enabled`, `checked`, `focused`, `scrollable`. Селектор: `text`, `text_contains`, `id` (целиком или короткое имя), `desc`,
`desc_contains`, `class`, `package`, `checked`, `enabled`, `focused`, `clickable`, `scrollable`, `selected`, `ignore_case`, `any` (список вариантов «или»);
неизвестное поле это ошибка.

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
| `List` | | `Phones {phones, details}`; `details`: для каждого `{phone, state, transport, address, serial, name, display, error?}`; `error {code, detail}` есть у телефона, который не готов (подсказка по состоянию или последняя ошибка демона) |
| `SetConnectPolicy` | `serial?`, `prefer?` (`usb`/`adb`/`serial`/`auto`), `fallback_secs?`, `scan_secs?`, `if_unset?` | правила выбора пути на ходу: предпочтение (для телефона или общее), через сколько секунд пробовать другой путь (0: не переключаться), период поиска; `if_unset`: не менять, если для телефона `serial` предпочтение уже задано. Ответ `Ok`, а при `serial` и `prefer` `ConnectPolicy {chosen}`: `same` (уже так), `switching` (переключаем), `connecting` (подключаем) |
| `Health` | | `Health {server, protocol, phones, errors}`; `phones` как `details` в `List`; `errors`: последние 100 ошибок `{at, phone, kind, detail}` (обрыв связи, предупреждения, битые пакеты, сбои команд над телефоном) |
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
| `DeviceInfo` | `phone` | `DeviceResult`: `manufacturer`, `model`, `android`, `sdk`, `abis`, `serial`, `screen_w`, `screen_h`, `dpi` |
| `DeviceStats` | `phone` | `battery` (`level`, `temperature_c`, `charging`), `cpu_percent`, `mem_total_kb`, `mem_available_kb` |
| `AppInfo` | `phone`, `packages` | `apps`: по пакету `installed`, `version_code`, `version_name`, `enabled`, `first_install`, `last_update` |
| `ForceStop`, `ClearAppData` | `phone`, `package` | остановить приложение; стереть его данные |
| `UninstallApp` | `phone`, `package`, `keep_data?` | удалить приложение |
| `InstallApk` | `phone`, `path`, `allow_downgrade?`, `sha256?` | установить APK, уже лежащий на телефоне в разрешённой папке (сначала `FilePut`); `sha256` проверяется на телефоне; ошибка несёт причину `pm` |
| `Logcat` | `phone`, `lines?` (500, до 5000), `package?`, `level?` (`V D I W E F`) | `data.text` |
| `SetMobileData` | `phone`, `on` | мобильные данные |
| `SystemDialog` | `phone`, `action?` (`allow`/`deny`) | `data.present`, `data.package`, `data.texts`, `data.buttons`, `data.target` (нажатая кнопка) |
| `ExitInfo` | `phone`, `package` | `data.text`: `dumpsys activity exit-info` |
| `SetPackageVerifier` | `phone`, `on` | проверка приложений при установке по ADB |
| `GetClipboard`, `SetClipboard` | `phone` / `phone`, `text` | `data.text`; по ADB ошибка, нужен компаньон |
| `CompanionPermissions` | `phone` | выдать компаньону права по ADB; `data.granted`, `data.failed` |
| `CompanionCall` | `phone`, `call` (`{"op": …}`) | команда прямо компаньону по USB-аксессуару (см. PROTOCOL.md §9), `data` ответа компаньона |
| `CompanionRestart` | `phone` | остановить и запустить приложение-компаньон; `data.accessibility_was_on`: была ли включена служба спецвозможностей |
| `CompanionInstall` | `phone`, `path`, `sha256?` | `InstallApk` APK компаньона, затем права и запуск; ответ как у `CompanionPermissions` |
| `PublicIp` | `phone`, `url?` | `data.ip`; `url`: свой адрес проверки (http/https, в ответе адрес в тексте) |
| `SetWifi` | `phone`, `on` | `data.previous`: был ли Wi-Fi включён |
| `StayAwake` | `phone`, `on` | `data.previous`: прежнее значение настройки |
| `ScreenState` | `phone` | `wakefulness`, `on`, `locked` |
| `Button` | `phone`, `name` | `back home recents menu power wakeup sleep volume_up volume_down volume_mute enter delete tab` |
| `LaunchApp` | `phone`, `package`, `activity?` | запустить приложение (пакет из блок-листа отклоняется) |
| `ListApps` | `phone`, `all?` | `data.packages`: со значком запуска, с `all` все |
| `ForegroundApp` | `phone` | `data.package`, `data.activity`, `data.blocked` |
| `UiDump`, `UiElements` | `phone` | `data.elements`: все элементы экрана / готовый список для показа (`kind`: `button field switch image list text other`) |
| `UiFind` | `phone`, `selector` | `data.elements` по селектору |
| `UiAt`, `UiFocused` | `phone`, `x`, `y` / `phone` | `data.element`: под точкой / поле ввода в фокусе |
| `WaitFor` | `phone`, `selector`, `timeout?`, `gone?` | ждать появления (исчезновения) элемента; `data.ok`, `data.elements` |
| `TypeText` | `phone`, `text` | набрать текст в поле в фокусе (по ADB печатный ASCII) |
| `NetworkInfo` | `phone` | `data.online`, `data.local_ip`, `data.interface` |
| `SetAirplaneMode` | `phone`, `on` | режим полёта |
| `SetBlocklist` | `packages` | блок-лист пакетов (маска `*` в начале или конце), общий на демон; `Ok` |
| `SetStream` | `phone, video, audio` | |
| `FilePut` | `phone, dest, size, media?` | `FileReady {transfer}`, затем данные клиента (тип 3), итог `FileDone`/`FileFailed` |
| `FileGet` | `phone, src` | `FileInfo {transfer, size}`, данные сервера (тип 3), итог `FileDone`/`FileFailed` |
| `FileList` | `phone, path` | `Files {phone, path, names}` |
| `FileDelete` | `phone, path` | `Ok` |
| `FileCancel` | `transfer` | `Ok` |

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
| `ScriptParams` | `script` | `ScriptParams {script, params:[{name, type, required, default, description}]}` из комментариев `-- @param` |
| `RunChain` | `steps, rounds?, round_pause?, name?, follow?` | `ChainStarted {chain, name}`; затем события цепочки |
| `Chains` | | `Chains {chains}` |
| `ChainInfo` | `chain` | `Chain {chain, summary, report}` |
| `StopChain` | `chain` | `Ok` |
| `ScheduleList` | | `Schedules {schedules}` |
| `SchedulePut` | `schedule?, name, rule, target, enabled?, skip_if_running?` | `ScheduleSaved {schedule}` |
| `ScheduleDelete` | `schedule` | `Ok` |
| `Artifacts` | `run` | `Artifacts {run, names}` |
| `Artifact` | `run, name` | `Artifact {run, name, size}` и пакет типа 4: `[длина имени u8][имя][содержимое]` |

`Run`: `phones` телефоны, выделяемые прогону (по умолчанию все готовые); телефон занимается эксклюзивно, пока прогон идёт; `args` параметры, доступные
сценарию как `args`; `tests` выполнить только тесты с этими именами; `timeout` срок прогона в секундах; `stay_awake` не давать экранам гаснуть на время
прогона (прежняя настройка возвращается); `device_logs` в конце сохранить `logcat` каждого телефона в папку прогона (`phone-N-logcat.txt`), `exit_info` список пакетов, для которых сохранить `ExitInfo` (`phone-N-exit-ПАКЕТ.txt`); `wifi_during_run` (`true` или список серийных номеров) включить Wi-Fi на время прогона и вернуть прежнее состояние после; `check_url` адрес проверки IP для `public_ip()` и `wait_online` (в Lua это глобальная `check_url`); `phones` принимает и серийные номера строками; `record` записывать экраны с таким числом кадров в секунду (до 30): `rec-N.mp4`, если на ПК есть `ffmpeg`, иначе кадры JPEG в `rec-N/`. Сбой любого из них идёт в журнал прогона предупреждением. Если в сценарии есть `-- @param`, `args` проверяются по схеме (тип, обязательность, лишние поля) и дополняются значениями по умолчанию.

`RunChain`: `steps` список `{script, args?, phones?, on_fail?, retries?, pause?, timeout?}`, `rounds` кругов (1), `round_pause` секунд между кругами.
`on_fail`: `stop` (по умолчанию), `continue`, `retry` (повторить `retries` раз, потом остановить). События цепочки приходят как `RunEvent` с `run` = номер цепочки:
`chain_start`, `chain_round {round}`, `chain_step {round, step, script, attempt, step_run, status}`, `chain_end {status}`. Итог в `chains/<номер>/chain.json`.
`Subscribe` принимает и номер цепочки. Всем клиентам приходят `ChainStarted` и `ChainEnded`.

`SchedulePut`: `rule` это `{"every": секунды}`, `{"daily": "ЧЧ:ММ", "days": [1..7], "utc_offset_min": 180}` (1 понедельник) или `{"at": время_мс}`; `target` это
`{"script": id, "args"?, "phones"?}` или `{"chain": описание}`. Расписание хранится в `schedules.json` и переживает перезапуск; пропущенные за простой запуски не
наверстываются; пока предыдущий запуск идёт, новый пропускается (`skip_if_running: false` отключает).

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

## Роли токенов

`pcdockd --role-token РОЛЬ=ТОКЕН` (можно повторять) добавляет токен с ограничением; основной токен остаётся полным. Роли: `view` (список, просмотр, чтение сведений, поиск по экрану), `input` (то же и ввод: касания, клавиши, кнопки, текст), `nofiles` (всё, кроме передачи файлов). Запрещённая команда отвечает `Error` с названием роли.

## Запись сценария

Касания человека на телефоне (по ADB, `getevent`) превращаются в шаги и готовый Lua. Интерфейс не нужен: команды может слать и скрипт.

| Команда | Параметры | Ответ |
|---|---|---|
| `RecordStart` | `phone` | `RecordState`; дальше событие `RecordState` при каждом изменении |
| `RecordPause` | `phone`, `on?` (по умолчанию `true`; `false`: продолжить) | `RecordState` |
| `RecordState` | `phone` | `RecordState {state}` |
| `RecordList` | | `RecordList {records}`: снимки всех идущих записей |
| `RecordSetSteps` | `phone`, `steps: [{id, t, v, warn?, src, pkg, time?}]` | `RecordState`. Порядок как в списке; `id: 0` новый шаг; не названные шаги удалены; у шага с новыми `t`/`v` Lua строится заново |
| `RecordScreen` | `phone` | `RecordScreen {num, elements: [{idx, kind, label}]}`: свежий список элементов экрана |
| `RecordAddCheck` | `phone`, `num`, `idx` | `RecordState` с шагом `check` («на экране видно»): цель берётся из текста, подписи или id элемента |
| `RecordDiscard` | `phone` | `Ok`: запись убрана без сохранения |
| `RecordStop` | `phone`, `name?`, `origin?` | `RecordResult {name, origin, lua, problem, steps}`; `problem`: первый шаг, который не перевёлся в Lua (такие шаги в сценарий не входят) |

Снимок `state`: `status` (`recording`, `paused`, `stopped`), `steps` (`id, t` вид шага, `time` секунды от начала, `v`, `warn`, `src`, `pkg`, `lua`, `problem`), `busy`, `notice`, `foreground`, `started_at`, `app`, `default_name`. Виды шагов: `tap` (`x`, `y`, `target?`), `long` (`x`, `y`, `hold`), `swipe` (`x1`, `y1`, `x2`, `y2`, `duration`), `button` (`name`), `text` (`text`), `check` (`target`), `wait` (`seconds`). Ошибки: `not_found: …` (записи нет), `bad_request: …` (неверные шаги или элемент), `failed: …` (экран не прочитался).
