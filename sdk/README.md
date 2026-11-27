# SDK (Rust)

Исходники продукта на Rust. Документация: [`docs/`](docs/README.md); примеры клиентов: [`examples/`](examples/pcdock.py). Общий обзор: [`../README.md`](../README.md).

| Крейт | Что это |
|---|---|
| `pcdock-core` | ядро: протокол ПК-телефон, сессия, HID, ввод; не знает ОС и не делает ввод-вывод |
| `pcdock-usb`, `pcdock-adb`, `pcdock-serial` | транспорты |
| `pcdock-farm` | ферма телефонов (внутренний крейт) |
| `pcdock-net`, `pcdock-serve`, `pcdock-client` | основа демонов, протокол фермы, клиент протокола |
| `pcdockd` | демон фермы |
| `pcdock-autod` | демон автоматизации (Lua) |
| `pcdock-check` | самопроверка |

```bash
cargo build --release     # бинарники в target/release/
cargo test                # около 170 тестов (телефоны заменены фейками)
```
