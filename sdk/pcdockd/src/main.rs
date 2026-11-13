//! `pcdockd` — демон док-станции.
//!
//! Режимы: `--auto` находит всё сам (USB, ADB, serial-трубы),
//! без него — явный список `--phone`. С `--auto` работают `--except`
//! и `--no-usb/--no-adb` (serial — только с `--serial`); без `--auto` `--phone` это «только эти».

use clap::Parser;
use pcdock_farm::{Farm, FarmConfig, PhoneSpec};
use pcdock_serve::Serve;

/// Демон PC Dock: трубки на любых транспортах, один протокол наружу.
#[derive(Debug, Parser)]
#[command(name = "pcdockd", version)]
struct Args {
    /// Найти всё сам: USB, ADB, serial-трубы.
    #[arg(long, short = 'a')]
    auto: bool,

    /// Стартовать без телефонов: их закрепляет клиент командой `Pin`.
    #[arg(long)]
    manual: bool,

    /// Закреплённый телефон: принимается всегда. Формат: `usb` (любой USB-телефон),
    /// `usb:1-2.3` (на этом USB-порту), `usb:СЕРИЙНИК`, `adb:СЕРИЙНИК`, `serial:ПУТЬ`.
    /// Без --auto это «только эти». Можно несколько раз.
    #[arg(long = "phone")]
    phones: Vec<String>,

    /// Пропустить при --auto: семейство (`usb`), `usb:1-2.3`, `adb:СЕРИЙНИК` или серийник.
    #[arg(long = "except")]
    except: Vec<String>,

    /// APK серверного процесса для ADB-режима (`server-adb-debug.apk`): без диалога записи экрана.
    #[arg(long = "server-apk")]
    server_apk: Option<std::path::PathBuf>,

    /// Включить передачу файлов на телефоны и с них (по ADB). Без этого файловые команды отключены.
    #[arg(long = "files")]
    files: bool,

    /// Папка на телефоне, доступная для файлов (можно несколько раз). По умолчанию: /sdcard,
    /// /storage/emulated/0, /data/local/tmp.
    #[arg(long = "files-root")]
    files_roots: Vec<String>,

    /// Адрес сервера ADB (по умолчанию локальный 127.0.0.1:5037).
    #[arg(long = "adb-server")]
    adb_server: Option<String>,

    /// Дополнительный производитель USB (VID, шестнадцатеричный, например `0x2a70`). Можно несколько раз.
    #[arg(long = "vid", value_parser = parse_vid)]
    vids: Vec<u16>,

    /// Путь для отдельного телефона: `СЕРИЙНИК=usb|adb|serial` (важнее общего порядка). Можно несколько раз.
    #[arg(long = "prefer", value_parser = parse_prefer)]
    prefer: Vec<(String, pcdock_core::link::Transport)>,

    /// Не переходить на другой путь, если выбранный не стал рабочим (приложения нет, нет прав).
    #[arg(long = "no-fallback")]
    no_fallback: bool,

    /// Выключить семейство при --auto.
    #[arg(long = "no-usb")]
    no_usb: bool,
    #[arg(long = "no-adb")]
    no_adb: bool,
    /// Искать serial-трубы (ttyACM, ttyUSB). По умолчанию выключено: порты открываются и получают байты
    /// протокола, а на них могут сидеть чужие устройства.
    #[arg(long = "serial")]
    serial: bool,

    /// Пароль для клиентов (первое сообщение `{"op":"Auth","token":"…"}`). Можно задать переменной
    /// `PCDOCK_TOKEN`. Без него слушать можно только на localhost.
    #[arg(long)]
    token: Option<String>,

    /// Слушать TCP (протокол с конвертом). Пусто = не слушать.
    #[arg(long, default_value = "127.0.0.1:9001")]
    tcp: Option<String>,
    #[arg(long = "no-tcp")]
    no_tcp: bool,

    /// Слушать веб-сокеты (браузеры). Пусто = не слушать.
    #[arg(long, default_value = "127.0.0.1:9002")]
    ws: Option<String>,
    #[arg(long = "no-ws")]
    no_ws: bool,

    /// Локальная труба (unix-сокет / именованный пайп). Пусто = не слушать.
    #[arg(long, default_value = "pcdock")]
    local: Option<String>,
    #[arg(long = "no-local")]
    no_local: bool,

    /// Имя хоста в HELLO.
    #[arg(long, default_value = "pcdockd/1.0")]
    name: String,

    /// Подробные логи (-v debug, -vv trace). Без флага — info.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,

    /// Видео/звук по умолчанию для новых трубок.
    #[arg(long, default_value_t = true)]
    video: bool,
    #[arg(long, default_value_t = false)]
    audio: bool,
}

/// Адрес только для этого компьютера (`127.0.0.1:…`, `localhost:…`, `[::1]:…`).
fn is_loopback(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    matches!(host.trim_matches(['[', ']']), "127.0.0.1" | "localhost" | "::1") || host.starts_with("127.")
}

fn parse_prefer(s: &str) -> Result<(String, pcdock_core::link::Transport), String> {
    use pcdock_core::link::Transport;
    let (serial, t) = s.split_once('=').ok_or_else(|| format!("{s:?}: нужно СЕРИЙНИК=usb|adb|serial"))?;
    let t = match t {
        "usb" => Transport::Usb,
        "adb" => Transport::Adb,
        "serial" => Transport::Serial,
        _ => return Err(format!("{s:?}: транспорт usb, adb или serial")),
    };
    Ok((serial.to_string(), t))
}

fn parse_vid(s: &str) -> Result<u16, String> {
    u16::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
        .map_err(|_| format!("{s:?}: нужен VID в шестнадцатеричном виде, например 0x2a70"))
}

fn main() {
    let args = Args::parse();
    if std::env::var("RUST_LOG").is_err() {
        let level = match args.verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        };
        // SAFETY: однопоточный старт, гонок нет.
        unsafe { std::env::set_var("RUST_LOG", level) };
    }
    env_logger::init();
    let mut pinned = Vec::new();
    for spec in &args.phones {
        match PhoneSpec::parse(spec) {
            Ok(p) => pinned.push(p),
            Err(e) => {
                log::error!("--phone {spec}: {e}");
                std::process::exit(2);
            }
        }
    }
    if !args.auto && pinned.is_empty() && !args.manual {
        log::error!("нужен --auto или хотя бы один --phone (см. --help)");
        std::process::exit(2);
    }
    let mut vendor_ids = FarmConfig::default().vendor_ids;
    for v in &args.vids {
        if !vendor_ids.contains(v) {
            vendor_ids.push(*v);
        }
    }
    let cfg = FarmConfig {
        vendor_ids,
        server_apk: args.server_apk.clone(),
        adb_server: args.adb_server.clone(),
        prefer: args.prefer.iter().cloned().collect(),
        fallback_after: if args.no_fallback { None } else { FarmConfig::default().fallback_after },
        name: args.name.clone(),
        video: args.video,
        audio: args.audio,
        auto: args.auto,
        manual: args.manual,
        except: args.except.clone(),
        pinned,
        usb: !args.no_usb,
        adb: !args.no_adb,
        serial: args.serial,
        ..FarmConfig::default()
    };
    // Ферма стартует пустой: телефоны подхватываются по мере появления (plug&play).
    let daemon = match Farm::from_config(cfg) {
        Ok((farm, warnings)) => {
            for w in warnings {
                log::warn!("{w}");
            }
            farm
        }
        Err(e) => {
            log::error!("{e}");
            std::process::exit(2);
        }
    };

    let token = args.token.clone().or_else(|| std::env::var("PCDOCK_TOKEN").ok()).filter(|t| !t.is_empty());
    // Без пароля наружу не слушаем: по сокету можно управлять телефоном и смотреть его экран.
    if token.is_none() {
        for (name, addr) in [("--tcp", &args.tcp), ("--ws", &args.ws)] {
            let on = (name == "--tcp" && !args.no_tcp) || (name == "--ws" && !args.no_ws);
            if let (true, Some(a)) = (on, addr.as_deref()) {
                if !a.is_empty() && !is_loopback(a) {
                    log::error!("{name} {a}: слушать не на localhost можно только с паролем (--token или PCDOCK_TOKEN)");
                    std::process::exit(2);
                }
            }
        }
    }
    let files = args.files.then(|| {
        pcdock_serve::FileOptions::adb(
            args.adb_server.clone(),
            (!args.files_roots.is_empty()).then(|| args.files_roots.clone()),
        )
    });
    if let Some(f) = &files {
        log::info!("передача файлов включена, папки: {}", f.roots.join(", "));
    }
    let serve = Serve::with_options(daemon, token, files);
    if !args.no_tcp {
        match args.tcp.as_deref().unwrap_or("") {
            "" => {}
            addr => match serve.run_tcp(addr) {
                Ok(bound) => log::info!("TCP: {bound}"),
                Err(e) => log::error!("TCP {addr} не встал: {e}"),
            },
        }
    }
    if !args.no_ws {
        match args.ws.as_deref().unwrap_or("") {
            "" => {}
            addr => match serve.run_ws(addr) {
                Ok(bound) => log::info!("WS: {bound}"),
                Err(e) => log::error!("WS {addr} не встал: {e}"),
            },
        }
    }
    if !args.no_local {
        match args.local.as_deref().unwrap_or("") {
            "" => {}
            name => match serve.run_local(name) {
                Ok(()) => log::info!("local: {name}"),
                Err(e) => log::error!("local {name} не встал: {e}"),
            },
        }
    }

    log::info!("pcdockd работает, трубки: {:?}", serve.daemon().lock().unwrap().phones());
    // Воркеры и слушатели живут в своих потоках; главный ждёт сигнала завершения (Ctrl+C, SIGTERM).
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    if let Err(e) = ctrlc::set_handler(move || {
        let _ = stop_tx.send(());
    }) {
        log::warn!("обработчик завершения не встал ({e}): при остановке телефоны не будут возвращены в обычный режим");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    let _ = stop_rx.recv();
    log::info!("завершаюсь: останавливаю телефоны и возвращаю их в обычный режим");
    // Подменяем ферму пустой и выбрасываем прежнюю: потоки телефонов останавливаются, USB сбрасывается.
    let farm = serve.daemon().clone();
    let old = std::mem::replace(&mut *farm.lock().unwrap(), Farm::new());
    drop(old);
    log::info!("готово");
}

#[cfg(test)]
mod tests {
    use super::is_loopback;

    #[test]
    fn loopback_detection() {
        for a in ["127.0.0.1:9001", "localhost:1", "[::1]:9", "127.0.0.2:5"] {
            assert!(is_loopback(a), "{a}");
        }
        for a in ["0.0.0.0:9001", "192.168.1.5:9001", ":9001", "example.com:1"] {
            assert!(!is_loopback(a), "{a}");
        }
    }
}
