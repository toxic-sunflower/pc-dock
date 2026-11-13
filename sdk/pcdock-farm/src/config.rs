//! Готовая настройка фермы с USB и ADB: из `FarmConfig` получается ферма с обнаружением и фабрикой каналов.
//! Демон и C ABI превращают свои входы (аргументы, JSON) в `FarmConfig` и больше ничего не решают.

use std::time::Duration;

use pcdock_core::link::{Discovery, LinkFactory, PhoneLink, Sighting, Transport};
use pcdock_core::packet::SetStream;
use pcdock_core::session::Config;
use pcdock_usb::{PhoneSelector, Settings, UsbLink, UsbPort, ANDROID_VIDS};

use crate::runtime::{DiscoveryConfig, Farm, PhoneSpec, Rules};

/// Настройка фермы.
#[derive(Clone, Debug)]
pub struct FarmConfig {
    /// Имя хоста в HELLO.
    pub name: String,
    /// Видео и звук по умолчанию для новых телефонов.
    pub video: bool,
    pub audio: bool,
    /// Брать всё найденное (кроме `except`).
    pub auto: bool,
    /// Исключения для `auto` (см. [`Rules`]).
    pub except: Vec<String>,
    /// Закреплённые телефоны: принимаются всегда, транспорт задан правилом.
    pub pinned: Vec<PhoneSpec>,
    /// Разрешить старт без автообнаружения и без закреплённых: телефоны закрепляет клиент на ходу (`Farm::pin`).
    pub manual: bool,
    /// Какие семейства искать.
    pub usb: bool,
    pub adb: bool,
    pub serial: bool,
    /// Производители USB (VID), которые считаются телефонами.
    pub vendor_ids: Vec<u16>,
    /// Адрес ADB-сервера; `None` — локальный.
    pub adb_server: Option<String>,
    /// Первый локальный порт для проброса ADB; следующие телефоны берут порты выше.
    pub adb_local_port: u16,
    /// APK серверного процесса для ADB-режима (`server-adb`). Без него ADB-телефоны не подключаются.
    pub server_apk: Option<std::path::PathBuf>,
    /// Скорость последовательных труб (у USB-CDC не влияет).
    pub serial_baud: u32,
    pub every: Duration,
    pub lost_after: Duration,
    /// Предпочтение транспортов, если один телефон виден по нескольким.
    pub order: Vec<Transport>,
    /// Предпочтение для отдельных телефонов: серийник → транспорт (важнее общего порядка).
    pub prefer: std::collections::HashMap<String, Transport>,
    /// Через сколько перейти на другой путь, если выбранный не стал рабочим (`None` — не переходить).
    pub fallback_after: Option<Duration>,
}

impl Default for FarmConfig {
    fn default() -> Self {
        let d = DiscoveryConfig::default();
        Self {
            name: "pcdock/1.0".into(),
            video: true,
            audio: false,
            auto: true,
            except: Vec::new(),
            pinned: Vec::new(),
            manual: false,
            usb: true,
            adb: true,
            serial: false,
            vendor_ids: ANDROID_VIDS.to_vec(),
            adb_server: None,
            adb_local_port: 47000,
            serial_baud: pcdock_serial::DEFAULT_BAUD,
            server_apk: std::env::var_os("PCDOCK_SERVER_APK").map(Into::into),
            every: d.every,
            lost_after: d.lost_after,
            order: d.order,
            prefer: d.prefer,
            fallback_after: d.fallback_after,
        }
    }
}

#[cfg(feature = "json")]
impl FarmConfig {
    /// Настройка из JSON-объекта. Все ключи необязательны, неизвестный ключ — ошибка (опечатки не молчат):
    /// `name`, `video`, `audio`, `auto`, `except` (строки), `phones` (строки `PhoneSpec`), `usb`, `adb`, `serial`,
    /// `vendor_ids` (числа), `adb_server`, `adb_local_port`, `every_ms`, `lost_after_ms`,
    /// `order` (`"usb"`, `"adb"`, `"serial"`).
    pub fn from_json(text: &str) -> Result<Self, String> {
        use serde_json::Value;
        let root: Value = serde_json::from_str(text).map_err(|e| format!("конфиг не JSON: {e}"))?;
        let obj = root.as_object().ok_or("конфиг должен быть JSON-объектом")?;
        let mut cfg = Self::default();
        let bool_of = |k: &str, v: &Value| v.as_bool().ok_or_else(|| format!("{k}: нужно true/false"));
        let str_of =
            |k: &str, v: &Value| v.as_str().map(String::from).ok_or_else(|| format!("{k}: нужна строка"));
        let strs_of = |k: &str, v: &Value| -> Result<Vec<String>, String> {
            v.as_array()
                .ok_or_else(|| format!("{k}: нужен массив строк"))?
                .iter()
                .map(|x| str_of(k, x))
                .collect()
        };
        let num_of =
            |k: &str, v: &Value| v.as_u64().ok_or_else(|| format!("{k}: нужно неотрицательное число"));
        for (k, v) in obj {
            match k.as_str() {
                "name" => cfg.name = str_of(k, v)?,
                "video" => cfg.video = bool_of(k, v)?,
                "audio" => cfg.audio = bool_of(k, v)?,
                "auto" => cfg.auto = bool_of(k, v)?,
                "usb" => cfg.usb = bool_of(k, v)?,
                "adb" => cfg.adb = bool_of(k, v)?,
                "serial" => cfg.serial = bool_of(k, v)?,
                "except" => cfg.except = strs_of(k, v)?,
                "phones" => {
                    cfg.pinned =
                        strs_of(k, v)?.iter().map(|s| PhoneSpec::parse(s)).collect::<Result<_, _>>()?
                }
                "vendor_ids" => {
                    cfg.vendor_ids = v
                        .as_array()
                        .ok_or("vendor_ids: нужен массив чисел")?
                        .iter()
                        .map(|x| {
                            x.as_u64()
                                .and_then(|n| u16::try_from(n).ok())
                                .ok_or_else(|| "vendor_ids: числа 0..65535".to_string())
                        })
                        .collect::<Result<_, _>>()?
                }
                "adb_server" => cfg.adb_server = Some(str_of(k, v)?),
                "serial_baud" => {
                    cfg.serial_baud =
                        u32::try_from(num_of(k, v)?).map_err(|_| "serial_baud: слишком много".to_string())?
                }
                "server_apk" => cfg.server_apk = Some(str_of(k, v)?.into()),
                "adb_local_port" => {
                    cfg.adb_local_port =
                        u16::try_from(num_of(k, v)?).map_err(|_| "adb_local_port: 0..65535".to_string())?
                }
                "every_ms" => cfg.every = Duration::from_millis(num_of(k, v)?),
                "lost_after_ms" => cfg.lost_after = Duration::from_millis(num_of(k, v)?),
                "prefer" => {
                    let map =
                        v.as_object().ok_or("prefer: нужен объект {\"СЕРИЙНИК\": \"usb|adb|serial\"}")?;
                    for (serial, t) in map {
                        let t = match t.as_str() {
                            Some("usb") => Transport::Usb,
                            Some("adb") => Transport::Adb,
                            Some("serial") => Transport::Serial,
                            _ => return Err(format!("prefer.{serial}: нужно usb, adb или serial")),
                        };
                        cfg.prefer.insert(serial.clone(), t);
                    }
                }
                "fallback_after_ms" => {
                    let ms = num_of(k, v)?;
                    cfg.fallback_after = if ms == 0 { None } else { Some(Duration::from_millis(ms)) };
                }
                "order" => {
                    cfg.order = strs_of(k, v)?
                        .iter()
                        .map(|s| match s.as_str() {
                            "usb" => Ok(Transport::Usb),
                            "adb" => Ok(Transport::Adb),
                            "serial" => Ok(Transport::Serial),
                            other => Err(format!("order: неизвестный транспорт {other:?}")),
                        })
                        .collect::<Result<_, _>>()?
                }
                other => return Err(format!("неизвестный ключ конфига: {other:?}")),
            }
        }
        Ok(cfg)
    }
}

impl FarmConfig {
    fn session(&self) -> Config {
        Config {
            name: self.name.clone(),
            stream: Some(SetStream { video: self.video, audio: self.audio, ..Default::default() }),
            ..Default::default()
        }
    }
}

impl Farm {
    /// Ферма по настройке. Возвращает предупреждения запуска (например, USB-обнаружение не встало):
    /// ферма при этом работает с тем, что удалось поднять.
    pub fn from_config(cfg: FarmConfig) -> Result<(Farm, Vec<String>), String> {
        if !cfg.auto && cfg.pinned.is_empty() && !cfg.manual {
            return Err("нужно автообнаружение или хотя бы один закреплённый телефон".into());
        }
        let mut warnings = Vec::new();
        let mut providers: Vec<Box<dyn Discovery>> = Vec::new();
        if cfg.usb {
            match pcdock_usb::UsbDiscovery::new(cfg.vendor_ids.clone()) {
                Ok(d) => providers.push(Box::new(d)),
                Err(e) => warnings.push(format!("USB-обнаружение не встало: {e}")),
            }
        }
        if cfg.adb {
            let adb = match &cfg.adb_server {
                Some(addr) => pcdock_adb::Adb::new(addr.clone()),
                None => pcdock_adb::Adb::local(),
            };
            providers.push(Box::new(pcdock_adb::AdbDiscovery::new(adb)));
        }
        if cfg.serial {
            providers.push(Box::new(pcdock_serial::SerialDiscovery));
        }
        let discovery = DiscoveryConfig {
            rules: Rules { auto: cfg.auto, except: cfg.except.clone(), pinned: cfg.pinned.clone() },
            every: cfg.every,
            lost_after: cfg.lost_after,
            order: cfg.order.clone(),
            prefer: cfg.prefer.clone(),
            fallback_after: cfg.fallback_after,
        };
        let factory = DefaultFactory {
            session: cfg.session(),
            next_adb_port: cfg.adb_local_port,
            adb_server: cfg.adb_server.clone(),
            server_apk: cfg.server_apk.clone(),
            serial_baud: cfg.serial_baud,
        };
        let mut farm = Farm::new();
        farm.enable_discovery(providers, Box::new(factory), discovery);
        Ok((farm, warnings))
    }
}

/// Создаёт канал по факту обнаружения: USB-аксессуар или ADB.
struct DefaultFactory {
    session: Config,
    next_adb_port: u16,
    adb_server: Option<String>,
    server_apk: Option<std::path::PathBuf>,
    serial_baud: u32,
}

impl LinkFactory for DefaultFactory {
    fn make(&mut self, s: &Sighting) -> Result<Box<dyn PhoneLink>, String> {
        match s.transport {
            Transport::Usb => {
                let port: UsbPort = s.address.parse()?;
                let target = PhoneSelector { port: Some(port), serial: s.serial.clone() };
                let link = UsbLink::new(
                    self.session.clone(),
                    Settings { target: Some(target), ..Settings::default() },
                )?;
                Ok(Box::new(link))
            }
            Transport::Adb => {
                let local = self.next_adb_port;
                self.next_adb_port = self.next_adb_port.checked_add(1).unwrap_or(47000);
                let apk = self.server_apk.clone().ok_or(
                    "для ADB нужен APK сервера: ключ server_apk (pcdockd --server-apk) или переменная PCDOCK_SERVER_APK",
                )?;
                let adb = match &self.adb_server {
                    Some(addr) => pcdock_adb::Adb::new(addr.clone()),
                    None => pcdock_adb::Adb::local(),
                };
                let link = pcdock_adb::AdbLink::new(self.session.clone(), s.address.clone(), local)
                    .with_server(adb, pcdock_adb::ServerSpec { apk, socket: "pcdock".into() });
                Ok(Box::new(link))
            }
            Transport::Serial => Ok(Box::new(pcdock_serial::SerialLink::open(
                self.session.clone(),
                s.address.clone(),
                self.serial_baud,
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "json")]
    #[test]
    fn json_config_parses_and_rejects_typos() {
        let cfg = FarmConfig::from_json(
            r#"{"auto": false, "phones": ["adb:ABC", "usb:1-2.3"], "except": ["usb"], "audio": true,
                "every_ms": 500, "order": ["adb", "usb"], "vendor_ids": [6353]}"#,
        )
        .unwrap();
        assert!(!cfg.auto && cfg.audio);
        assert_eq!(cfg.pinned.len(), 2);
        assert_eq!(cfg.every, Duration::from_millis(500));
        assert_eq!(cfg.order, vec![Transport::Adb, Transport::Usb]);
        assert_eq!(cfg.vendor_ids, vec![6353]);
        assert!(FarmConfig::from_json("{}").is_ok());
        assert!(FarmConfig::from_json(r#"{"autto": true}"#).is_err());
        assert!(FarmConfig::from_json(r#"{"phones": ["bt:1"]}"#).is_err());
        assert!(FarmConfig::from_json("[1]").is_err());
        assert!(FarmConfig::from_json(r#"{"vendor_ids": [70000]}"#).is_err());
    }

    #[test]
    fn config_needs_auto_or_pinned() {
        let none = FarmConfig { auto: false, usb: false, adb: false, serial: false, ..FarmConfig::default() };
        assert!(Farm::from_config(none).is_err());
        let pinned = FarmConfig {
            auto: false,
            usb: false,
            adb: false,
            serial: false,
            pinned: vec![PhoneSpec::parse("adb:ABC").unwrap()],
            ..FarmConfig::default()
        };
        let (farm, _) = Farm::from_config(pinned).unwrap();
        assert!(farm.phones().is_empty(), "ферма стартует пустой");
    }
}
