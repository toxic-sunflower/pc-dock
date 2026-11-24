//! `pcdock-autod`: демон автоматизации PC Dock. Подключается к `pcdockd`, хранит сценарии на Lua, запускает их как
//! автотесты и показывает ход прогона в прямом эфире.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use pcdock_autod::backend::{FarmTarget, ReconnectBackend};
use pcdock_autod::runs::Runs;
use pcdock_autod::service;
use pcdock_autod::store::ScriptStore;

/// Демон автоматизации PC Dock.
#[derive(Debug, Parser)]
#[command(name = "pcdock-autod", version)]
struct Args {
    /// Где `pcdockd`: `host:port`, `tcp://host:port` или `local:имя` (локальная труба).
    #[arg(long, default_value = "127.0.0.1:9001")]
    farm: String,

    /// Пароль `pcdockd` (или переменная `PCDOCK_FARM_TOKEN`).
    #[arg(long)]
    farm_token: Option<String>,

    /// Частота кадров, которую автоматизация просит у телефона, когда сценарию нужны кадры.
    #[arg(long, default_value_t = 5.0)]
    frame_fps: f32,

    /// Папка данных: сценарии (`scripts/`), прогоны (`runs/`).
    #[arg(long, default_value = "pcdock-autod-data")]
    data: PathBuf,

    /// Пароль для клиентов (или переменная `PCDOCK_TOKEN`). Без него слушать можно только на localhost.
    #[arg(long)]
    token: Option<String>,

    /// Слушать TCP. Пусто = не слушать.
    #[arg(long, default_value = "127.0.0.1:9101")]
    tcp: Option<String>,
    #[arg(long = "no-tcp")]
    no_tcp: bool,

    /// Слушать веб-сокеты. Пусто = не слушать.
    #[arg(long, default_value = "127.0.0.1:9102")]
    ws: Option<String>,
    #[arg(long = "no-ws")]
    no_ws: bool,

    /// Локальная труба. Пусто = не слушать.
    #[arg(long, default_value = "pcdock-autod")]
    local: Option<String>,
    #[arg(long = "no-local")]
    no_local: bool,

    /// Подробные логи (-v debug, -vv trace).
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,
}

/// Адрес только для этого компьютера.
fn is_loopback(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    matches!(host.trim_matches(['[', ']']), "127.0.0.1" | "localhost" | "::1") || host.starts_with("127.")
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

    let token = args.token.clone().or_else(|| std::env::var("PCDOCK_TOKEN").ok()).filter(|t| !t.is_empty());
    if token.is_none() {
        for (name, addr, on) in [("--tcp", &args.tcp, !args.no_tcp), ("--ws", &args.ws, !args.no_ws)] {
            if let (true, Some(a)) = (on, addr.as_deref()) {
                if !a.is_empty() && !is_loopback(a) {
                    log::error!("{name} {a}: слушать не на localhost можно только с паролем (--token или PCDOCK_TOKEN)");
                    std::process::exit(2);
                }
            }
        }
    }
    let target = match FarmTarget::parse(&args.farm) {
        Ok(t) => t,
        Err(e) => {
            log::error!("--farm: {e}");
            std::process::exit(2);
        }
    };
    let store = match ScriptStore::open(&args.data) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            log::error!("папка данных: {e}");
            std::process::exit(2);
        }
    };
    let farm_token =
        args.farm_token.clone().or_else(|| std::env::var("PCDOCK_FARM_TOKEN").ok()).filter(|t| !t.is_empty());
    let backend = ReconnectBackend::start(target, farm_token, args.frame_fps);
    let runs = match Runs::new(&args.data, store.clone(), backend.clone()) {
        Ok(r) => r,
        Err(e) => {
            log::error!("{e}");
            std::process::exit(2);
        }
    };
    let hub = service::build(store, runs.clone(), backend, token);

    if !args.no_tcp {
        match args.tcp.as_deref().unwrap_or("") {
            "" => {}
            addr => match hub.run_tcp(addr) {
                Ok(bound) => log::info!("TCP: {bound}"),
                Err(e) => log::error!("TCP {addr} не встал: {e}"),
            },
        }
    }
    if !args.no_ws {
        match args.ws.as_deref().unwrap_or("") {
            "" => {}
            addr => match hub.run_ws(addr) {
                Ok(bound) => log::info!("WS: {bound}"),
                Err(e) => log::error!("WS {addr} не встал: {e}"),
            },
        }
    }
    if !args.no_local {
        match args.local.as_deref().unwrap_or("") {
            "" => {}
            name => match hub.run_local(name) {
                Ok(()) => log::info!("local: {name}"),
                Err(e) => log::error!("local {name} не встал: {e}"),
            },
        }
    }
    log::info!("pcdock-autod работает, данные: {}", args.data.display());

    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    // Родитель умер (pynelo закрылся аварийно или не успел остановить нас) — завершаемся сами, как по SIGTERM:
    // иначе сирота держит USB телефонов и следующий запуск родителя их не видит. Включается переменной
    // PCDOCK_PARENT_PID=<pid родителя>; опрос раз в 2 с (getppid меняется, когда родителя не стало).
    #[cfg(unix)]
    if let Some(want) = std::env::var("PCDOCK_PARENT_PID").ok().and_then(|s| s.parse::<u32>().ok()) {
        let tx = stop_tx.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            if std::os::unix::process::parent_id() != want {
                log::warn!("родитель {want} завершился — завершаюсь");
                let _ = tx.send(());
                return;
            }
        });
    }
    if ctrlc::set_handler(move || {
        let _ = stop_tx.send(());
    })
    .is_err()
    {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    let _ = stop_rx.recv();
    log::info!("завершаюсь: останавливаю идущие прогоны");
    runs.stop_all();
    std::thread::sleep(std::time::Duration::from_millis(500));
}
