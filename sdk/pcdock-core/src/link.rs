//! Общий интерфейс транспорта одного телефона и команды ввода к нему.
//!
//! Транспорты (USB, ADB, serial) реализуют [`PhoneLink`]; ферма владеет ими и про тип
//! подключения не знает. Здесь только описание, ввода-вывода в ядре нет.

use crate::input::{DeviceKind, MouseButton, TouchPhase};
use crate::session::Event;

/// Идентификатор трубки. Назначает вызывающий.
pub type PhoneId = u32;

/// Команда ввода одной трубке. Человеческий темп (паузы между DOWN/MOVE/UP,
/// мелкие порции скролла) делает вызывающий; либа шлёт как есть.
#[derive(Clone, Debug)]
pub enum InputCmd {
    DeviceEnable {
        kind: DeviceKind,
        on: bool,
    },
    Touch {
        cid: u8,
        phase: TouchPhase,
        x: f32,
        y: f32,
        width: Option<f32>,
        height: Option<f32>,
        pressure: Option<f32>,
    },
    CursorMoveAbs {
        x: f32,
        y: f32,
    },
    CursorMoveRel {
        dx: i32,
        dy: i32,
    },
    CursorResync,
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    Scroll {
        ticks: f32,
    },
    ScrollH {
        ticks: f32,
    },
    Key {
        usage: u16,
        down: bool,
    },
    ReleaseAll,
    RequestFrame,
    Wake,
    /// Включить или выключить видео и звук; `max_fps` — не чаще стольких кадров в секунду (0 — без ограничения).
    SetStream {
        video: bool,
        audio: bool,
        max_fps: u8,
    },
    /// Запрос к приложению на телефоне (возможности 7–13). Ответ всегда приходит событием `Status` с кодом
    /// [`AppRequest::reply_code`]: от телефона или, если запрос не ушёл (телефон не умеет, неверный пакет или URL,
    /// сессия не активна), от самой сессии с `detail` = `"<request> error <причина>"` (для `UiDump` —
    /// `"<request> 1/1 error <причина>"`, как у частей ответа).
    App(AppRequest),
}

/// Запрос к приложению на телефоне. `request` — номер, который вернётся в начале `detail` ответа.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppRequest {
    /// Открыть приложение (`LAUNCH_APP`, ответ `STATUS` 18).
    LaunchApp { request: u32, package: String },
    /// Приложения с главным экраном (`LIST_APPS`, ответ `STATUS` 19).
    ListApps { request: u32 },
    /// Элементы экрана в виде uiautomator XML (`UI_DUMP`, ответ `STATUS` 20 частями).
    UiDump { request: u32 },
    /// Причины завершения процессов приложения после `since_ms` (`EXIT_INFO`, ответ `STATUS` 22).
    ExitInfo { request: u32, since_ms: u64, package: String },
    /// HTTPS GET по сети телефона: внешний IP (`IP_CHECK`, ответ `STATUS` 23).
    IpCheck { request: u32, url: String },
    /// HTTP-запрос к устройству локальной Wi-Fi-сети телефона (`HTTP_LOCAL`, ответ `STATUS` 24): например, админка
    /// роутера, в Wi-Fi которого телефон. `method` — `GET`/`POST`, `url` — `http(s)://` с IPv4 частной сети,
    /// `headers` — по порядку, `body` — только у `POST` (≤ 64 КБ), `timeout_ms` — 0 = 10 с, не больше 30 с.
    /// Ответ `"<request> <JSON>"`: `{"ok":true,"code","body","headers","ms"}` или `{"ok":false,"error","ms"}`.
    HttpLocal {
        request: u32,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: String,
        timeout_ms: u32,
    },
    /// Ввести текст (любой язык, эмодзи) в поле с курсором (`TEXT_INPUT`, ответ `STATUS` 25): не пустой,
    /// ≤ 16 КБ UTF-8. Ответ `"<request> <JSON>"`: `{"ok":true,"method":"paste"|"set_text"|"clipboard","ms"}` или
    /// `{"ok":false,"error","ms"}`; `"clipboard"` — текст в буфере обмена, Ctrl+V жмёт хост.
    TypeText { request: u32, text: String },
}

impl AppRequest {
    /// Код `STATUS`, которым придёт ответ.
    pub fn reply_code(&self) -> u16 {
        match self {
            Self::LaunchApp { .. } => 18,
            Self::ListApps { .. } => 19,
            Self::UiDump { .. } => 20,
            Self::ExitInfo { .. } => 22,
            Self::IpCheck { .. } => 23,
            Self::HttpLocal { .. } => 24,
            Self::TypeText { .. } => 25,
        }
    }

    pub fn request(&self) -> u32 {
        match self {
            Self::LaunchApp { request, .. }
            | Self::ListApps { request }
            | Self::UiDump { request }
            | Self::ExitInfo { request, .. }
            | Self::IpCheck { request, .. }
            | Self::HttpLocal { request, .. }
            | Self::TypeText { request, .. } => *request,
        }
    }
}

impl InputCmd {
    /// Касание с явными пятном/силой. `None` = default ядра.
    #[allow(clippy::too_many_arguments)]
    pub fn touch_ex(
        cid: u8,
        phase: TouchPhase,
        x: f32,
        y: f32,
        width: Option<f32>,
        height: Option<f32>,
        pressure: Option<f32>,
    ) -> Self {
        Self::Touch { cid, phase, x, y, width, height, pressure }
    }
}

/// Ошибка связи. `Gone` = устройство пропало, надо переподключать.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    Gone(String),
    Fatal(String),
    /// Работать нельзя, пока человек что-то не сделает (см. [`PhoneState::hint`]).
    Blocked(PhoneState, String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone(e) => write!(f, "устройство пропало: {e}"),
            Self::Fatal(e) => write!(f, "{e}"),
            Self::Blocked(_, e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// Связь с одной трубкой. Реализации: USB сегодня, ADB и serial-труба завтра.
/// Сессия ядра живёт внутри реализации; наружу идут готовые события.
pub trait PhoneLink: Send {
    fn is_connected(&self) -> bool;
    /// Найти устройство и поднять сессию. `Gone` = повторить позже.
    fn connect(&mut self) -> Result<(), LinkError>;
    /// Один шаг: отправить накопленное, прочитать входящее, вернуть события.
    fn poll(&mut self) -> Result<Vec<Event>, LinkError>;
    /// Применить команду ввода (уйдёт при следующем `poll`).
    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError>;
    /// Аккуратно отпустить устройство.
    fn shutdown(&mut self);
}

/// Каким путём телефон подключён.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Transport {
    /// USB-аксессуар (AOA).
    Usb,
    /// ADB-сервер на телефоне.
    Adb,
    /// Последовательная труба от микроконтроллера.
    Serial,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Usb => "usb",
            Self::Adb => "adb",
            Self::Serial => "serial",
        })
    }
}

/// Факт «этот телефон сейчас виден таким путём». Обнаружение выдаёт факты, решения принимает ферма.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sighting {
    pub transport: Transport,
    /// Адрес в пределах транспорта: порт USB (`1-2.3`), серийник ADB, путь serial.
    pub address: String,
    /// Серийник телефона, если известен: по нему ферма узнаёт один и тот же телефон на разных транспортах.
    pub serial: Option<String>,
    /// Что мешает работать, если телефон виден, но пользоваться им нельзя.
    pub issue: Option<Issue>,
}

/// Почему виден телефон, но подключиться к нему пока нельзя.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Issue {
    /// Нет прав на USB-устройство (Linux: правило udev; Windows: драйвер WinUSB).
    NoAccess,
    /// ADB: компьютер не подтверждён на телефоне («Разрешить отладку»).
    Unauthorized,
}

impl Issue {
    pub fn state(self) -> PhoneState {
        match self {
            Self::NoAccess => PhoneState::NeedsAccess,
            Self::Unauthorized => PhoneState::NeedsAuth,
        }
    }
}

impl Sighting {
    /// Ключ телефона: серийник, а без него — путь транспорта.
    pub fn key(&self) -> String {
        match &self.serial {
            Some(s) => format!("serial:{s}"),
            None => format!("{}:{}", self.transport, self.address),
        }
    }
}

/// Источник фактов о телефонах (скан USB, опрос ADB…). Вызывается из потока владельца фермы.
pub trait Discovery: Send {
    fn scan(&mut self) -> Vec<Sighting>;

    /// Источник знает, что что-то изменилось (подключили или отключили устройство), и просит опросить
    /// его раньше срока. Вызов сбрасывает признак. Источники без событий всегда отвечают `false`.
    fn changed(&mut self) -> bool {
        false
    }
}

/// Создаёт канал к телефону по факту обнаружения.
pub trait LinkFactory: Send {
    fn make(&mut self, sighting: &Sighting) -> Result<Box<dyn PhoneLink>, String>;
}

/// Состояние телефона для потребителя.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhoneState {
    /// Только что замечен.
    Appeared,
    /// Идёт подключение или переподключение.
    Connecting,
    /// Рукопожатие прошло.
    Ready,
    /// Приложение запущено, но захвата экрана нет (нужно разрешение на телефоне).
    NeedsPermission,
    /// Телефон перешёл в режим аксессуара, но приложение PC Dock не отвечает (не установлено или не запущено).
    NeedsApp,
    /// Нет прав на USB-устройство.
    NeedsAccess,
    /// ADB ждёт подтверждения компьютера на телефоне.
    NeedsAuth,
    /// Пропал с обнаружения.
    Lost,
}

impl PhoneState {
    /// Что сказать пользователю в этом состоянии (пусто, если ничего делать не нужно).
    pub fn hint(self) -> &'static str {
        match self {
            Self::NeedsPermission => "Разрешите запись экрана на телефоне (диалог «Начать запись»).",
            Self::NeedsApp => {
                "Телефон в режиме аксессуара, но приложение PC Dock не отвечает: оно не установлено, не запущено или \
                 ждёт ответа на экране (подтвердить запуск для аксессуара, разрешить запись экрана). Посмотрите на телефон."
            }
            Self::NeedsAccess => {
                "Нет доступа к USB-устройству: на Linux добавьте правило udev, на Windows поставьте драйвер WinUSB \
                 (см. sdk/README.md, «Первое подключение»)."
            }
            Self::NeedsAuth => {
                "Подтвердите на телефоне «Разрешить отладку по USB с этого компьютера» (отметьте «Всегда разрешать»)."
            }
            _ => "",
        }
    }
}
