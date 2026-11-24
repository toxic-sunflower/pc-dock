# Демон `pcdockd`

Демон держит ферму в отдельном процессе и отдаёт её другим процессам и браузерам. Автоматизация (сценарии, тесты) живёт в отдельном демоне: [autod.md](autod.md). Он ничего не решает про телефоны:
аргументы превращает в `FarmConfig`, события фермы рассылает клиентам, команды клиентов передаёт ферме.

## Запуск

```bash
pcdockd --auto --server-apk /opt/pcdock/server-adb-debug.apk
```

Без аргументов демон завершается с кодом 2 и просьбой указать `--auto` или `--phone`.

### Параметры

| Параметр | Назначение |
|---|---|
| `-a`, `--auto` | принимать всё найденное (USB, ADB) |
| `--manual` | стартовать без телефонов: клиент закрепляет их на ходу командой `Pin` (телефон не трогается, пока его не попросили) |
| `--phone <спец>` | закреплённый телефон (можно несколько раз): `usb`, `usb:1-2.3`, `usb:СЕРИЙНИК`, `adb:СЕРИЙНИК`, `serial:ПУТЬ`. Без `--auto` это «только эти» |
| `--except <спец>` | исключить при `--auto`: семейство, `usb:1-2.3`, `adb:СЕРИЙНИК`, серийник |
| `--server-apk <файл>` | APK серверного процесса для ADB (`server-adb-debug.apk`); или переменная `PCDOCK_SERVER_APK` |
| `--vid <0x....>` | добавить производителя USB (VID в шестнадцатеричном виде), можно несколько раз |
| `--prefer <СЕРИЙНИК=usb\|adb\|serial>` | путь для отдельного телефона, можно несколько раз |
| `--no-fallback` | не переходить на другой путь, если выбранный не стал рабочим |
| `--no-usb`, `--no-adb` | выключить семейство при `--auto` |
| `--serial` | искать serial-трубы (по умолчанию выключено) |
| `--token <пароль>` | пароль для клиентов; или переменная `PCDOCK_TOKEN` |
| `--tcp <адрес>` / `--no-tcp` | TCP (по умолчанию `127.0.0.1:9001`) |
| `--ws <адрес>` / `--no-ws` | WebSocket (по умолчанию `127.0.0.1:9002`) |
| `--local <имя>` / `--no-local` | локальная труба (по умолчанию `pcdock`) |
| `--name <имя>` | имя хоста в рукопожатии (по умолчанию `pcdockd/1.0`) |
| `--audio` | включить звук у новых телефонов |
| `--files` | включить передачу файлов на телефоны и с них (по ADB) |
| `--files-root <путь>` | папка на телефоне, доступная для файлов (можно несколько раз); по умолчанию `/sdcard`, `/storage/emulated/0`, `/data/local/tmp` |
| `--adb-server <адрес>` | адрес сервера ADB (по умолчанию локальный `127.0.0.1:5037`) |
| `-v`, `-vv` | подробные логи (`debug`, `trace`); также переменная `RUST_LOG` |

Известная особенность: параметр `--video` существует, но по умолчанию включён и выключить видео флагом нельзя (звук включается
`--audio`); управлять потоком можно командой клиента `SetStream`.

### Слушатели

- **TCP**: поток пакетов с конвертом (ниже). Для программ любого языка.
- **WebSocket**: текстовые фреймы — JSON, бинарные — кадры. Для браузеров.
- **Локальная труба**: имя по умолчанию `pcdock`. Linux: абстрактный unix-сокет `@pcdock`; Windows: именованный канал
  `\\.\pipe\pcdock`; macOS: сокет `/tmp/pcdock`.

**Безопасность:** по сокету можно смотреть экран и управлять телефоном. Без пароля демон слушает только `localhost`: попытка
указать другой адрес без `--token` заканчивается ошибкой запуска (код 2). С паролем клиент первым сообщением шлёт `Auth`.
Шифрования нет: наружу выставляйте только через защищённый туннель (SSH, VPN, обратный прокси с TLS).

## Протокол

Полное описание (конверт, версия, пароль, номера запросов, команды, события, видео-, звуковые и файловые пакеты) в [protocol.md](protocol.md).
Коротко: JSON-команды и события плюс бинарные пакеты, пароль первой командой, `id` в команде возвращается в ответе, кадры и звук подписчикам с условиями (`fps`, `on_change`).

## Пример клиента на Python (TCP)

```python
import json, socket, struct

s = socket.create_connection(("127.0.0.1", 9001))

def send(msg):
    body = b"\x00" + json.dumps(msg).encode()          # тип 0 = JSON
    s.sendall(struct.pack("<I", len(body)) + body)

def recv():
    n = struct.unpack("<I", s.recv(4, socket.MSG_WAITALL))[0]
    data = s.recv(n, socket.MSG_WAITALL)
    return data[0], data[1:]                            # (тип, тело)

kind, body = recv()
print(json.loads(body))                                 # {"ev":"Hello","protocol":1,...}
# send({"op": "Auth", "token": "..."})                  # если демон запущен с токеном
send({"op": "List"})
send({"op": "Watch", "phone": 1})
while True:
    kind, body = recv()
    if kind == 0:
        print(json.loads(body))
    elif kind == 1:
        phone, seq, codec, key, pts = struct.unpack_from("<IQBBQ", body)
        jpeg = body[22:]
        print(f"кадр {seq} от {phone}: {len(jpeg)} байт")
```

## Пример клиента в браузере (WebSocket)

```html
<img id="screen"><script>
const ws = new WebSocket("ws://127.0.0.1:9002");
ws.binaryType = "blob";
ws.onopen = () => ws.send(JSON.stringify({op: "Watch", phone: 1}));
ws.onmessage = async (e) => {
  if (typeof e.data === "string") { console.log(JSON.parse(e.data)); return; }
  const buf = await e.data.arrayBuffer();
  if (new Uint8Array(buf)[0] !== 1) return;                         // 1 = видео, 2 = звук
  document.getElementById("screen").src = URL.createObjectURL(new Blob([buf.slice(1 + 22)], {type: "image/jpeg"}));
};
// нажатие по картинке: доли от её размера
document.getElementById("screen").onclick = (ev) => {
  const r = ev.target.getBoundingClientRect(), x = (ev.clientX - r.left) / r.width, y = (ev.clientY - r.top) / r.height;
  for (const phase of [0, 2]) ws.send(JSON.stringify({op: "TouchEx", phone: 1, cid: 0, phase, x, y, width: null, height: null, pressure: null}));
};
</script>
```
