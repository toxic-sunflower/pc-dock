//! Живая ферма: владеет телефонами, их потоками и переподключениями.
//!
//! Абстракция для встраивания: добавил телефон, положил команду, забрал событие.
//! Потоки, транспорты и ретраи внутри, снаружи их не видно. Sans-IO реестр сессий
//! (`Farm`) живёт рядом и нужен тем, кто приносит ввод-вывод сам.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pcdock_core::link::{
    Discovery, InputCmd, Issue, LinkError, LinkFactory, PhoneId, PhoneLink, PhoneState, Sighting, Transport,
};
use pcdock_core::session::Event;

/// Код STATUS «проекция остановлена» (PROTOCOL.md): приложение живо, захвата экрана нет.
const STATUS_PROJECTION_STOPPED: u16 = 1;

/// Кадр с номером. Номер растёт на каждый кадр трубки: клиент сокетов
/// видит пропуск при заторе.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedFrame {
    pub seq: u64,
    pub codec: u8,
    pub key_frame: bool,
    pub pts_us: u64,
    pub data: Vec<u8>,
}

/// Событие трубки наружу. Видео идёт только если трубку смотрят
/// (`set_watching`), кэш последнего кадра — всегда.
#[derive(Clone, Debug)]
pub enum FarmEvent {
    Connected {
        phone: PhoneId,
        version: u16,
        capabilities: u32,
        name: String,
    },
    Display {
        phone: PhoneId,
        stream_w: u16,
        stream_h: u16,
        screen_w: u16,
        screen_h: u16,
        dpi: u16,
        rotation: u8,
    },
    Video {
        phone: PhoneId,
        frame: CachedFrame,
    },
    AudioConfig {
        phone: PhoneId,
        codec: u8,
        rate: u32,
        channels: u8,
    },
    Audio {
        phone: PhoneId,
        pts_us: u64,
        data: Vec<u8>,
    },
    Status {
        phone: PhoneId,
        code: u16,
        detail: String,
    },
    PeerSilent {
        phone: PhoneId,
    },
    Malformed {
        phone: PhoneId,
        detail: String,
    },
    Disconnected {
        phone: PhoneId,
        reason: String,
    },
    /// Связь потеряна, воркер переподключает. Отдельно от `Disconnected` сессии.
    LinkDown {
        phone: PhoneId,
        detail: String,
    },
    Warning {
        phone: PhoneId,
        detail: String,
    },
    /// Обнаружение заметило новый телефон (или вернувшийся).
    Appeared {
        phone: PhoneId,
        transport: Transport,
        address: String,
        serial: Option<String>,
    },
    /// Состояние телефона изменилось.
    State {
        phone: PhoneId,
        state: PhoneState,
        /// Подсказка человеку (пусто, если делать ничего не нужно).
        detail: String,
    },
    /// Телефон пропал надолго.
    Lost {
        phone: PhoneId,
    },
}

enum WorkerCmd {
    Input(InputCmd),
    Stop,
}

struct Worker {
    cmd_tx: Sender<WorkerCmd>,
    handle: Option<JoinHandle<()>>,
}

/// Размер и поворот экрана телефона, как в `Display`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayMeta {
    pub stream_w: u16,
    pub stream_h: u16,
    pub screen_w: u16,
    pub screen_h: u16,
    pub dpi: u16,
    pub rotation: u8,
}

/// Сведения о телефоне для списка (`Farm::phones_info`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhoneInfo {
    pub id: PhoneId,
    pub state: PhoneState,
    /// Путь, по которому телефон найден обнаружением (`None` у телефонов, добавленных вручную).
    pub transport: Option<Transport>,
    pub address: Option<String>,
    pub serial: Option<String>,
    /// Имя приложения или сервера на телефоне из рукопожатия.
    pub name: Option<String>,
    pub display: Option<DisplayMeta>,
}

struct Slot {
    worker: Worker,
    state: PhoneState,
    meta: PhoneInfo,
    watching: bool,
    last_frame: Option<CachedFrame>,
    frame_seq: u64,
}

/// Демон. Не потокобезопасен сам: держите в одном потоке-владельце.
pub struct Farm {
    slots: HashMap<PhoneId, Slot>,
    ev_tx: Sender<FarmEvent>,
    ev_rx: Receiver<FarmEvent>,
    auto: Option<Auto>,
    /// Телефоны, снятые `unpin`: `Lost` о них отдаёт ближайший `drain`.
    pending_lost: Vec<PhoneId>,
}

/// Закреплённый телефон: правило «этот телефон должен быть». Частный случай обнаружения: подходящие
/// факты принимаются даже при выключенном автообнаружении, а транспорт задан правилом.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhoneSpec {
    /// `None` — любой транспорт.
    pub transport: Option<Transport>,
    /// Порт USB, серийник ADB или путь serial.
    pub address: Option<String>,
    /// Серийник телефона.
    pub serial: Option<String>,
}

impl PhoneSpec {
    /// Разбирает `usb` (любой USB-телефон), `usb:1-2.3` (порт), `usb:СЕРИЙНИК`, `adb:СЕРИЙНИК`, `serial:ПУТЬ`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (family, what) = match spec.split_once(':') {
            Some((f, w)) => (f, Some(w)),
            None => (spec, None),
        };
        let transport = match family {
            "usb" => Transport::Usb,
            "adb" => Transport::Adb,
            "serial" => Transport::Serial,
            _ => {
                return Err(format!(
                "неизвестный формат телефона {spec:?} (usb, usb:ПОРТ|СЕРИЙНИК, adb:СЕРИЙНИК, serial:ПУТЬ)"
            ))
            }
        };
        let mut out = Self { transport: Some(transport), address: None, serial: None };
        match what {
            None => {}
            Some("") => return Err(format!("пусто после двоеточия в {spec:?}")),
            Some(w) if transport == Transport::Usb && !looks_like_port(w) => out.serial = Some(w.into()),
            Some(w) => out.address = Some(w.into()),
        }
        if transport == Transport::Adb && out.address.is_none() {
            return Err("для adb нужен серийник: adb:СЕРИЙНИК".into());
        }
        Ok(out)
    }

    /// Назван ли этим правилом именно тот телефон, что виден как `s` (по серийнику, а для ADB — по адресу).
    fn names(&self, s: &Sighting) -> bool {
        let serial = s.serial.as_deref();
        (self.serial.is_some() && self.serial.as_deref() == serial)
            || (self.transport == Some(Transport::Adb)
                && self.address.is_some()
                && self.address.as_deref() == serial)
    }

    pub fn matches(&self, s: &Sighting) -> bool {
        self.transport.is_none_or(|t| t == s.transport)
            && self.address.as_ref().is_none_or(|a| *a == s.address)
            && self.serial.as_ref().is_none_or(|v| s.serial.as_ref() == Some(v))
    }
}

/// Порт USB выглядит как `1-2` или `1-2.3`; всё остальное считаем серийником.
fn looks_like_port(s: &str) -> bool {
    let Some((bus, ports)) = s.split_once('-') else { return false };
    !bus.is_empty()
        && bus.chars().all(|c| c.is_ascii_digit())
        && !ports.is_empty()
        && ports.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Какие факты обнаружения принимает ферма.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    /// Брать всё, что найдено (кроме `except`).
    pub auto: bool,
    /// Исключения для `auto`: семейство (`usb`), `usb:1-2.3`, `adb:СЕРИЙНИК` или просто серийник.
    pub except: Vec<String>,
    /// Закреплённые телефоны принимаются всегда.
    pub pinned: Vec<PhoneSpec>,
}

impl Default for Rules {
    fn default() -> Self {
        Self { auto: true, except: Vec::new(), pinned: Vec::new() }
    }
}

impl Rules {
    pub fn accepts(&self, s: &Sighting) -> bool {
        if self.pinned.iter().any(|p| p.matches(s)) {
            return true;
        }
        // Телефон закреплён за транспортом (`adb:СЕРИЙНИК`): по другим путям его не берём, даже при `auto`.
        if self.pinned.iter().any(|p| p.names(s)) {
            return false;
        }
        let by_address = format!("{}:{}", s.transport, s.address);
        self.auto
            && !self.except.iter().any(|e| {
                *e == s.transport.to_string() || *e == by_address || s.serial.as_deref() == Some(e.as_str())
            })
    }
}

/// Настройки автообнаружения.
#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    /// Что принимать.
    pub rules: Rules,
    /// Как часто опрашивать источники (проверяется при каждом `drain`).
    pub every: Duration,
    /// Сколько телефон может не показываться, прежде чем считаться потерянным. Больше времени (в WSL через usbipd
    /// переподключение после сброса USB занимает до 40 с, на Linux/macOS 1–3 с),
    /// которое телефон отсутствует при переходе в режим аксессуара и обратно (несколько секунд).
    pub lost_after: Duration,
    /// Предпочтение транспортов, когда один телефон виден по нескольким: раньше в списке — важнее.
    pub order: Vec<Transport>,
    /// Предпочтение для отдельного телефона (серийник → транспорт): важнее общего порядка.
    pub prefer: std::collections::HashMap<String, Transport>,
    /// Сколько ждать, пока выбранный путь станет рабочим (`needs_app`, `needs_access`), прежде чем
    /// попробовать другой путь, если телефон виден по нескольким. `None` — не переключаться.
    pub fallback_after: Option<Duration>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            rules: Rules::default(),
            every: Duration::from_secs(2),
            lost_after: Duration::from_secs(45),
            order: vec![Transport::Usb, Transport::Adb, Transport::Serial],
            prefer: Default::default(),
            fallback_after: Some(Duration::from_secs(20)),
        }
    }
}

struct Tracked {
    id: PhoneId,
    transport: Transport,
    last_seen: Instant,
    /// Телефон виден, но подключиться нельзя: вместо настоящего канала стоит заглушка.
    parked: Option<Issue>,
    /// С какого момента выбранный путь не рабочий (`needs_app`/`needs_access`).
    failing_since: Option<Instant>,
    /// Пути, которые уже пробовали и которые не сработали: к ним не возвращаемся, пока телефон не пропадёт.
    tried: Vec<Transport>,
}

/// Канал-заглушка для телефона, к которому пока нельзя подключиться (нет прав, нет подтверждения отладки):
/// ферма показывает состояние и подсказку, а настоящий канал ставит, когда помеха исчезнет.
struct ParkedLink(Issue);

impl PhoneLink for ParkedLink {
    fn is_connected(&self) -> bool {
        false
    }
    fn connect(&mut self) -> Result<(), LinkError> {
        Err(LinkError::Blocked(self.0.state(), "ждём действий пользователя".into()))
    }
    fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
        Ok(Vec::new())
    }
    fn input(&mut self, _: &InputCmd) -> Result<(), LinkError> {
        Ok(())
    }
    fn shutdown(&mut self) {}
}

struct Auto {
    providers: Vec<Box<dyn Discovery>>,
    factory: Box<dyn LinkFactory>,
    cfg: DiscoveryConfig,
    last_scan: Option<Instant>,
    /// Ключ телефона → слот, который ведёт обнаружение.
    tracked: HashMap<String, Tracked>,
    /// Ключ → выданный идентификатор: вернувшийся телефон получает прежний номер.
    ids: HashMap<String, PhoneId>,
    /// Какой путь доводил телефон до `ready` (в памяти, пока работает ферма): при следующем появлении
    /// берём его первым.
    good: HashMap<String, Transport>,
    /// Ключи, на которых фабрика уже ошиблась: предупреждаем один раз.
    failed: std::collections::HashSet<String>,
    /// Запрошенные вручную переключения пути (`Farm::set_transport`): выполняются при ближайшем обнаружении.
    switch: HashMap<PhoneId, Transport>,
}

impl Farm {
    pub fn new() -> Self {
        let (ev_tx, ev_rx) = mpsc::channel();
        Self { slots: HashMap::new(), ev_tx, ev_rx, auto: None, pending_lost: Vec::new() }
    }

    /// Канал событий наружу. Неблокирующий опрос — через `try_recv`.
    pub fn events(&self) -> &Receiver<FarmEvent> {
        &self.ev_rx
    }

    pub fn phones(&self) -> Vec<PhoneId> {
        let mut ids: Vec<PhoneId> = self.slots.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Добавляет трубку и поднимает её поток. Связь поднимется сама с ретраями.
    pub fn add_phone(&mut self, id: PhoneId, link: Box<dyn PhoneLink>) -> Result<(), String> {
        if self.slots.contains_key(&id) {
            return Err(format!("трубка {id} уже есть"));
        }
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let ev_tx = self.ev_tx.clone();
        let handle = thread::Builder::new()
            .name(format!("phone-{id}"))
            .spawn(move || worker_loop(id, link, cmd_rx, ev_tx))
            .map_err(|e| format!("не встал поток трубки {id}: {e}"))?;
        self.slots.insert(
            id,
            Slot {
                worker: Worker { cmd_tx, handle: Some(handle) },
                state: PhoneState::Connecting,
                meta: PhoneInfo {
                    id,
                    state: PhoneState::Connecting,
                    transport: None,
                    address: None,
                    serial: None,
                    name: None,
                    display: None,
                },
                watching: false,
                last_frame: None,
                frame_seq: 0,
            },
        );
        Ok(())
    }

    /// Убирает трубку: поток останавливается, кэш выбрасывается.
    pub fn remove_phone(&mut self, id: PhoneId) -> Result<(), String> {
        let mut slot = self.slots.remove(&id).ok_or_else(|| format!("нет трубки {id}"))?;
        let _ = slot.worker.cmd_tx.send(WorkerCmd::Stop);
        if let Some(h) = slot.worker.handle.take() {
            let _ = h.join();
        }
        Ok(())
    }

    /// Положить команду ввода в очередь трубки. Нет трубки = ошибка.
    pub fn command(&self, id: PhoneId, cmd: InputCmd) -> Result<(), String> {
        let slot = self.slots.get(&id).ok_or_else(|| format!("нет трубки {id}"))?;
        slot.worker.cmd_tx.send(WorkerCmd::Input(cmd)).map_err(|_| format!("поток трубки {id} мёртв"))?;
        Ok(())
    }

    pub fn set_watching(&mut self, id: PhoneId, watching: bool) -> Result<(), String> {
        self.slots.get_mut(&id).ok_or_else(|| format!("нет трубки {id}"))?.watching = watching;
        Ok(())
    }

    pub fn is_watching(&self, id: PhoneId) -> Result<bool, String> {
        self.slots.get(&id).ok_or_else(|| format!("нет трубки {id}")).map(|s| s.watching)
    }

    /// Последний кадр трубки. Очередь событий не трогает.
    pub fn last_frame(&self, id: PhoneId) -> Result<Option<CachedFrame>, String> {
        self.slots.get(&id).ok_or_else(|| format!("нет трубки {id}")).map(|s| s.last_frame.clone())
    }

    /// Включает автообнаружение: источники опрашиваются при `drain`, найденные телефоны
    /// получают канал от фабрики. Явно добавленные телефоны (`add_phone`) обнаружение не трогает.
    pub fn enable_discovery(
        &mut self,
        providers: Vec<Box<dyn Discovery>>,
        factory: Box<dyn LinkFactory>,
        cfg: DiscoveryConfig,
    ) {
        self.auto = Some(Auto {
            providers,
            factory,
            cfg,
            last_scan: None,
            tracked: HashMap::new(),
            ids: HashMap::new(),
            failed: Default::default(),
            good: HashMap::new(),
            switch: HashMap::new(),
        });
    }

    /// Перевести телефон на другой путь (например, с ADB на USB-аксессуар). Выполняется при ближайшем обнаружении:
    /// успех приходит событием `Appeared`, невозможность (путь телефона не виден) — `Warning`.
    pub fn set_transport(&mut self, id: PhoneId, transport: Transport) -> Result<(), String> {
        let auto = self.auto.as_mut().ok_or("обнаружение не включено")?;
        if !auto.tracked.values().any(|t| t.id == id) {
            return Err(format!("телефон {id} не найден обнаружением"));
        }
        auto.switch.insert(id, transport);
        auto.last_scan = None;
        Ok(())
    }

    /// Закрепить телефон на ходу: с ближайшего обнаружения ферма его берёт (даже без автообнаружения).
    /// Так приложение может подключать телефоны по одному по просьбе человека.
    pub fn pin(&mut self, spec: PhoneSpec) -> Result<(), String> {
        let auto = self.auto.as_mut().ok_or("обнаружение не включено")?;
        if !auto.cfg.rules.pinned.contains(&spec) {
            auto.cfg.rules.pinned.push(spec);
        }
        auto.last_scan = None;
        Ok(())
    }

    /// Снять закрепление. Телефоны, которые правила больше не принимают, отключаются (USB-аксессуар возвращается
    /// в обычный режим); возвращает их номера, событие `Lost` для каждого придёт из `drain`.
    pub fn unpin(&mut self, spec: &PhoneSpec) -> Result<Vec<PhoneId>, String> {
        let auto = self.auto.as_mut().ok_or("обнаружение не включено")?;
        let before = auto.cfg.rules.pinned.len();
        auto.cfg.rules.pinned.retain(|p| p != spec);
        if auto.cfg.rules.pinned.len() == before {
            return Err("такой телефон не закреплён".into());
        }
        let mut drop = Vec::new();
        for (key, t) in &auto.tracked {
            let meta = self.slots.get(&t.id).map(|s| &s.meta);
            let sight = Sighting {
                transport: t.transport,
                address: meta.and_then(|m| m.address.clone()).unwrap_or_default(),
                serial: meta.and_then(|m| m.serial.clone()),
                issue: None,
            };
            if spec.matches(&sight) && !auto.cfg.rules.accepts(&sight) {
                drop.push((key.clone(), t.id));
            }
        }
        let mut ids = Vec::new();
        for (key, id) in drop {
            auto.tracked.remove(&key);
            ids.push(id);
        }
        for id in &ids {
            let _ = self.remove_phone(*id);
            self.pending_lost.push(*id);
        }
        Ok(ids)
    }

    /// Сведения обо всех телефонах (по возрастанию номера).
    pub fn phones_info(&self) -> Vec<PhoneInfo> {
        let mut v: Vec<PhoneInfo> = self.slots.values().map(|s| s.meta.clone()).collect();
        v.sort_by_key(|i| i.id);
        v
    }

    /// Запоминает, каким путём найден телефон (вызывается при обнаружении).
    fn note_found(&mut self, id: PhoneId, transport: Transport, address: &str, serial: &Option<String>) {
        if let Some(slot) = self.slots.get_mut(&id) {
            slot.meta.transport = Some(transport);
            slot.meta.address = Some(address.to_string());
            if serial.is_some() {
                slot.meta.serial = serial.clone();
            }
        }
    }

    /// Состояние телефона.
    pub fn state(&self, id: PhoneId) -> Result<PhoneState, String> {
        self.slots.get(&id).ok_or_else(|| format!("нет трубки {id}")).map(|s| s.state)
    }

    /// Один проход обнаружения прямо сейчас (в `drain` он идёт по таймеру). События — в возврате.
    pub fn discover_now(&mut self) -> Vec<FarmEvent> {
        let mut out = Vec::new();
        self.run_discovery(true, &mut out);
        out
    }

    fn set_state(&mut self, phone: PhoneId, state: PhoneState, out: &mut Vec<FarmEvent>) {
        if let Some(slot) = self.slots.get_mut(&phone) {
            if slot.state != state {
                slot.state = state;
                slot.meta.state = state;
                out.push(FarmEvent::State { phone, state, detail: state.hint().to_string() });
            }
        }
    }

    fn run_discovery(&mut self, force: bool, out: &mut Vec<FarmEvent>) {
        let Some(mut auto) = self.auto.take() else { return };
        let now = Instant::now();
        // Источник с событиями просит опросить раньше срока (все спрашиваем, чтобы сбросить их признаки).
        let urgent = auto.providers.iter_mut().fold(false, |acc, p| p.changed() | acc);
        if !force && !urgent && auto.last_scan.is_some_and(|t| now.duration_since(t) < auto.cfg.every) {
            self.auto = Some(auto);
            return;
        }
        auto.last_scan = Some(now);

        // Факты, сгруппированные по телефону: один телефон может быть виден несколькими путями.
        let mut seen: HashMap<String, Vec<Sighting>> = HashMap::new();
        for p in auto.providers.iter_mut() {
            for s in p.scan() {
                if auto.cfg.rules.accepts(&s) {
                    seen.entry(s.key()).or_default().push(s);
                }
            }
        }
        // Сначала рабочие пути, потом заданный для телефона или вспомнивший успех, потом общий порядок.
        let good = auto.good.clone();
        let rank = |s: &Sighting| {
            let wanted =
                s.serial.as_ref().and_then(|v| auto.cfg.prefer.get(v)).or_else(|| good.get(&s.key()));
            (
                s.issue.is_some(),
                wanted.is_none_or(|t| *t != s.transport),
                auto.cfg.order.iter().position(|o| *o == s.transport).unwrap_or(usize::MAX),
            )
        };

        for (key, group) in &seen {
            let best = group.iter().min_by_key(|s| rank(s)).expect("группа не пуста").clone();
            match auto.tracked.get_mut(key) {
                Some(t) => {
                    t.last_seen = now;
                    let id = t.id;
                    if let Some(want) = auto.switch.remove(&id) {
                        if want != t.transport {
                            match group.iter().find(|s| s.transport == want && s.issue.is_none()) {
                                Some(alt) => match auto.factory.make(alt) {
                                    Ok(link) => {
                                        let _ = self.remove_phone(id);
                                        if self.add_phone(id, link).is_ok() {
                                            t.transport = want;
                                            t.parked = None;
                                            t.failing_since = None;
                                            t.tried.clear();
                                            auto.good.insert(key.clone(), want);
                                            self.note_found(id, want, &alt.address, &alt.serial);
                                            out.push(FarmEvent::Appeared {
                                                phone: id,
                                                transport: want,
                                                address: alt.address.clone(),
                                                serial: alt.serial.clone(),
                                            });
                                        }
                                    }
                                    Err(e) => out.push(FarmEvent::Warning { phone: id, detail: e }),
                                },
                                None => out.push(FarmEvent::Warning {
                                    phone: id,
                                    detail: format!("путь {want} сейчас недоступен"),
                                }),
                            }
                            continue;
                        }
                    }
                    if let Some(issue) = best.issue {
                        // Помеха появилась (или осталась): работающий канал заменяем заглушкой с подсказкой.
                        if t.parked != Some(issue) {
                            let _ = self.remove_phone(id);
                            if self.add_phone(id, Box::new(ParkedLink(issue))).is_ok() {
                                t.parked = Some(issue);
                                self.set_state(id, issue.state(), out);
                            }
                        }
                        continue;
                    }
                    if t.parked.is_some() {
                        // Помеха исчезла (права выданы, отладка подтверждена): ставим настоящий канал.
                        match auto.factory.make(&best) {
                            Ok(link) => {
                                let _ = self.remove_phone(id);
                                if self.add_phone(id, link).is_ok() {
                                    t.parked = None;
                                    t.transport = best.transport;
                                }
                            }
                            Err(e) => out.push(FarmEvent::Warning { phone: id, detail: e }),
                        }
                        continue;
                    }
                    // Выбранный путь не становится рабочим, а есть другой: после ожидания пробуем другой.
                    let state = self.slots.get(&id).map(|sl| sl.state);
                    match state {
                        Some(PhoneState::Ready) => {
                            t.failing_since = None;
                            auto.good.insert(key.clone(), t.transport);
                        }
                        Some(PhoneState::NeedsApp | PhoneState::NeedsAccess) => {
                            let since = *t.failing_since.get_or_insert(now);
                            if auto.cfg.fallback_after.is_some_and(|d| now.duration_since(since) >= d) {
                                let alt = group
                                    .iter()
                                    .filter(|s| s.issue.is_none() && s.transport != t.transport)
                                    .find(|s| !t.tried.contains(&s.transport))
                                    .cloned();
                                if let Some(alt) = alt {
                                    match auto.factory.make(&alt) {
                                        Ok(link) => {
                                            let _ = self.remove_phone(id);
                                            if self.add_phone(id, link).is_ok() {
                                                t.tried.push(t.transport);
                                                t.transport = alt.transport;
                                                t.failing_since = None;
                                                self.note_found(id, alt.transport, &alt.address, &alt.serial);
                                                out.push(FarmEvent::Appeared {
                                                    phone: id,
                                                    transport: alt.transport,
                                                    address: alt.address.clone(),
                                                    serial: alt.serial.clone(),
                                                });
                                            }
                                        }
                                        Err(e) => out.push(FarmEvent::Warning { phone: id, detail: e }),
                                    }
                                    continue;
                                }
                            }
                        }
                        _ => t.failing_since = None,
                    }
                    let still_there = group.iter().any(|s| s.transport == t.transport && s.issue.is_none());
                    if still_there {
                        continue;
                    }
                    // Прежний путь пропал, а другой есть: переводим телефон на него под тем же номером.
                    match auto.factory.make(&best) {
                        Ok(link) => {
                            let _ = self.remove_phone(id);
                            if self.add_phone(id, link).is_ok() {
                                t.transport = best.transport;
                                self.note_found(id, best.transport, &best.address, &best.serial);
                                out.push(FarmEvent::Appeared {
                                    phone: id,
                                    transport: best.transport,
                                    address: best.address.clone(),
                                    serial: best.serial.clone(),
                                });
                            }
                        }
                        Err(e) => out.push(FarmEvent::Warning { phone: id, detail: e }),
                    }
                }
                None => {
                    let remembered = auto.ids.get(key).copied();
                    let id = match remembered {
                        Some(id) if !self.slots.contains_key(&id) => id,
                        _ => (1..)
                            .find(|n| !self.slots.contains_key(n) && !auto.ids.values().any(|v| v == n))
                            .expect("кончились номера"),
                    };
                    let made: Result<Box<dyn PhoneLink>, String> = match best.issue {
                        Some(issue) => Ok(Box::new(ParkedLink(issue))),
                        None => auto.factory.make(&best),
                    };
                    match made {
                        Ok(link) => {
                            if self.add_phone(id, link).is_ok() {
                                auto.ids.insert(key.clone(), id);
                                auto.failed.remove(key);
                                auto.tracked.insert(
                                    key.clone(),
                                    Tracked {
                                        id,
                                        transport: best.transport,
                                        last_seen: now,
                                        parked: best.issue,
                                        failing_since: None,
                                        tried: Vec::new(),
                                    },
                                );
                                self.note_found(id, best.transport, &best.address, &best.serial);
                                out.push(FarmEvent::Appeared {
                                    phone: id,
                                    transport: best.transport,
                                    address: best.address.clone(),
                                    serial: best.serial.clone(),
                                });
                                if let Some(issue) = best.issue {
                                    self.set_state(id, issue.state(), out);
                                }
                            }
                        }
                        Err(e) => {
                            if auto.failed.insert(key.clone()) {
                                out.push(FarmEvent::Warning { phone: 0, detail: format!("{key}: {e}") });
                            }
                        }
                    }
                }
            }
        }

        // Давно не виденные телефоны считаем потерянными; номер запоминаем.
        let gone: Vec<String> = auto
            .tracked
            .iter()
            .filter(|(k, t)| !seen.contains_key(*k) && now.duration_since(t.last_seen) >= auto.cfg.lost_after)
            .map(|(k, _)| k.clone())
            .collect();
        for key in gone {
            if let Some(t) = auto.tracked.remove(&key) {
                self.set_state(t.id, PhoneState::Lost, out);
                let _ = self.remove_phone(t.id);
                out.push(FarmEvent::Lost { phone: t.id });
            }
        }
        self.auto = Some(auto);
    }

    /// Разобрать входящие события воркеров: кэш кадров, фильтр просмотра, состояния, обнаружение.
    /// Возвращает события для фронтенда (сокеты, окно, скрипты).
    pub fn drain(&mut self) -> Vec<FarmEvent> {
        let mut out = Vec::new();
        for phone in std::mem::take(&mut self.pending_lost) {
            out.push(FarmEvent::Lost { phone });
        }
        self.run_discovery(false, &mut out);
        while let Ok(ev) = self.ev_rx.try_recv() {
            let phone = match &ev {
                FarmEvent::Connected { phone, .. }
                | FarmEvent::Display { phone, .. }
                | FarmEvent::Video { phone, .. }
                | FarmEvent::AudioConfig { phone, .. }
                | FarmEvent::Audio { phone, .. }
                | FarmEvent::Status { phone, .. }
                | FarmEvent::PeerSilent { phone }
                | FarmEvent::Malformed { phone, .. }
                | FarmEvent::Disconnected { phone, .. }
                | FarmEvent::LinkDown { phone, .. }
                | FarmEvent::Warning { phone, .. }
                | FarmEvent::Appeared { phone, .. }
                | FarmEvent::State { phone, .. }
                | FarmEvent::Lost { phone } => *phone,
            };
            // События воркера про телефон, которого уже убрали (потеря, смена пути), отбрасываем.
            if !self.slots.contains_key(&phone) && !matches!(ev, FarmEvent::Lost { .. }) {
                continue;
            }
            let watching = self.slots.get(&phone).map(|s| s.watching).unwrap_or(false);
            match &ev {
                FarmEvent::Display { stream_w, stream_h, screen_w, screen_h, dpi, rotation, .. } => {
                    if let Some(slot) = self.slots.get_mut(&phone) {
                        slot.meta.display = Some(DisplayMeta {
                            stream_w: *stream_w,
                            stream_h: *stream_h,
                            screen_w: *screen_w,
                            screen_h: *screen_h,
                            dpi: *dpi,
                            rotation: *rotation,
                        });
                    }
                }
                FarmEvent::Connected { name, .. } => {
                    if let Some(slot) = self.slots.get_mut(&phone) {
                        slot.meta.name = Some(name.clone());
                    }
                    self.set_state(phone, PhoneState::Ready, &mut out)
                }
                FarmEvent::Status { code, .. } if *code == STATUS_PROJECTION_STOPPED => {
                    self.set_state(phone, PhoneState::NeedsPermission, &mut out)
                }
                FarmEvent::Video { .. }
                    if self.slots.get(&phone).is_some_and(|s| s.state == PhoneState::NeedsPermission) =>
                {
                    self.set_state(phone, PhoneState::Ready, &mut out)
                }
                FarmEvent::LinkDown { .. } | FarmEvent::Disconnected { .. } => {
                    // Пока человек не сделал нужное, «подключаемся» говорить неправда: оставляем причину.
                    let blocked = self.slots.get(&phone).is_some_and(|s| {
                        matches!(
                            s.state,
                            PhoneState::NeedsApp | PhoneState::NeedsAccess | PhoneState::NeedsAuth
                        )
                    });
                    if !blocked {
                        self.set_state(phone, PhoneState::Connecting, &mut out)
                    }
                }
                // Телефон молчит, хотя рукопожатия ещё не было: скорее всего, приложения нет или оно не запущено.
                FarmEvent::PeerSilent { .. }
                    if self.slots.get(&phone).is_some_and(|s| s.state == PhoneState::Connecting) =>
                {
                    self.set_state(phone, PhoneState::NeedsApp, &mut out)
                }
                // Состояние, о котором сообщил сам канал (например, нет прав на USB).
                FarmEvent::State { state, .. } => {
                    self.set_state(phone, *state, &mut out);
                    continue;
                }
                _ => {}
            }
            match ev {
                FarmEvent::Video { frame, .. } => {
                    if let Some(slot) = self.slots.get_mut(&phone) {
                        slot.frame_seq += 1;
                        let mut cached = frame;
                        cached.seq = slot.frame_seq;
                        slot.last_frame = Some(cached.clone());
                        if watching {
                            out.push(FarmEvent::Video { phone, frame: cached });
                        }
                    }
                }
                _ => out.push(ev),
            }
        }
        out
    }
}

impl Default for Farm {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Farm {
    fn drop(&mut self) {
        for (_, mut slot) in self.slots.drain() {
            let _ = slot.worker.cmd_tx.send(WorkerCmd::Stop);
            if let Some(h) = slot.worker.handle.take() {
                let _ = h.join();
            }
        }
    }
}

fn worker_loop(
    id: PhoneId,
    mut link: Box<dyn PhoneLink>,
    cmd_rx: Receiver<WorkerCmd>,
    ev_tx: Sender<FarmEvent>,
) {
    let send = |ev: FarmEvent| {
        let _ = ev_tx.send(ev);
    };
    // Одну и ту же причину не повторяем каждую секунду: сообщаем, когда она сменилась.
    let mut last_problem: Option<String> = None;
    let mut report = |err: LinkError| {
        let key = err.to_string();
        if last_problem.as_deref() == Some(key.as_str()) {
            return;
        }
        last_problem = Some(key);
        match err {
            LinkError::Gone(detail) => send(FarmEvent::LinkDown { phone: id, detail }),
            LinkError::Fatal(detail) => send(FarmEvent::Warning { phone: id, detail }),
            LinkError::Blocked(state, detail) => {
                send(FarmEvent::LinkDown { phone: id, detail });
                send(FarmEvent::State { phone: id, state, detail: state.hint().to_string() });
            }
        }
    };
    loop {
        // Команды забираем всегда, даже без связи: ввод копится в сессии.
        match cmd_rx.try_recv() {
            Ok(WorkerCmd::Stop) => {
                link.shutdown();
                return;
            }
            Ok(WorkerCmd::Input(cmd)) => {
                if let Err(e) = link.input(&cmd) {
                    send(FarmEvent::Warning { phone: id, detail: format!("ввод отклонён: {e}") });
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                link.shutdown();
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if !link.is_connected() {
            match link.connect() {
                Ok(()) => {}
                Err(e) => {
                    report(e);
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
            }
        }
        match link.poll() {
            Ok(events) => {
                for ev in events {
                    send(map_event(id, ev));
                }
            }
            Err(e) => {
                let gone = !matches!(e, LinkError::Fatal(_));
                report(e);
                if gone {
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }
}

fn map_event(id: PhoneId, ev: Event) -> FarmEvent {
    match ev {
        Event::Connected { version, capabilities, peer_name } => {
            FarmEvent::Connected { phone: id, version, capabilities, name: peer_name }
        }
        Event::Display(d) => FarmEvent::Display {
            phone: id,
            stream_w: d.stream_width,
            stream_h: d.stream_height,
            screen_w: d.screen_width,
            screen_h: d.screen_height,
            dpi: d.density_dpi,
            rotation: d.rotation,
        },
        Event::Video(f) => FarmEvent::Video {
            phone: id,
            frame: CachedFrame {
                seq: 0,
                codec: f.codec,
                key_frame: f.key_frame,
                pts_us: f.pts_us,
                data: f.data,
            },
        },
        Event::AudioConfig(c) => {
            FarmEvent::AudioConfig { phone: id, codec: c.codec, rate: c.sample_rate, channels: c.channels }
        }
        Event::Audio(f) => FarmEvent::Audio { phone: id, pts_us: f.pts_us, data: f.data },
        Event::Status(s) => FarmEvent::Status { phone: id, code: s.code, detail: s.detail },
        Event::PeerSilent => FarmEvent::PeerSilent { phone: id },
        Event::Malformed(detail) => FarmEvent::Malformed { phone: id, detail },
        Event::Disconnected(reason) => FarmEvent::Disconnected { phone: id, reason: format!("{reason:?}") },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcdock_core::packet::{DisplayInfo, Hello, Role, VideoFrame};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Фейковая связь без USB: скриптованные события, запись ввода.
    struct FakeLink {
        connected: bool,
        events: VecDeque<Event>,
        pub inputs: Arc<Mutex<Vec<String>>>,
        fail_connect: bool,
    }

    impl FakeLink {
        fn with_events(events: Vec<Event>) -> (Self, Arc<Mutex<Vec<String>>>) {
            let inputs = Arc::new(Mutex::new(Vec::new()));
            (
                Self { connected: false, events: events.into(), inputs: inputs.clone(), fail_connect: false },
                inputs,
            )
        }
    }

    impl PhoneLink for FakeLink {
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn connect(&mut self) -> Result<(), LinkError> {
            if self.fail_connect {
                return Err(LinkError::Gone("нет устройства".into()));
            }
            self.connected = true;
            Ok(())
        }
        fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
            let mut out = Vec::new();
            if let Some(ev) = self.events.pop_front() {
                out.push(ev);
            }
            Ok(out)
        }
        fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError> {
            self.inputs.lock().unwrap().push(format!("{cmd:?}"));
            Ok(())
        }
        fn shutdown(&mut self) {
            self.connected = false;
        }
    }

    fn hello() -> Event {
        Event::Connected { version: 2, capabilities: 0, peer_name: "fake".into() }
    }

    fn display() -> Event {
        Event::Display(DisplayInfo {
            stream_width: 360,
            stream_height: 820,
            screen_width: 720,
            screen_height: 1640,
            density_dpi: 320,
            rotation: 0,
        })
    }

    fn video(pts: u64) -> Event {
        Event::Video(VideoFrame { codec: 1, key_frame: true, pts_us: pts, data: vec![1, 2, 3] })
    }

    fn wait_for(d: &mut Farm, want_video: usize, timeout_ms: u64) -> Vec<FarmEvent> {
        let mut got = Vec::new();
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_millis(timeout_ms) {
            got.extend(d.drain());
            if got.iter().filter(|e| matches!(e, FarmEvent::Video { .. })).count() >= want_video {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        got
    }

    #[test]
    fn routes_commands_and_caches_frames_per_phone() {
        let mut d = Farm::new();
        let (l1, inputs1) = FakeLink::with_events(vec![hello(), display(), video(10), video(11)]);
        let (l2, inputs2) = FakeLink::with_events(vec![hello()]);
        d.add_phone(1, Box::new(l1)).unwrap();
        d.add_phone(2, Box::new(l2)).unwrap();
        assert_eq!(
            d.add_phone(1, Box::new(FakeLink::with_events(vec![]).0)).unwrap_err(),
            "трубка 1 уже есть"
        );
        d.set_watching(1, true).unwrap();

        d.command(1, InputCmd::Scroll { ticks: 2.0 }).unwrap();
        d.command(2, InputCmd::Wake).unwrap();
        assert!(d.command(99, InputCmd::Wake).is_err());

        let got = wait_for(&mut d, 2, 2000);
        assert!(got.iter().any(|e| matches!(e, FarmEvent::Connected { phone: 1, .. })));
        assert!(got.iter().any(|e| matches!(e, FarmEvent::Display { phone: 1, .. })));
        let vids: Vec<_> = got
            .iter()
            .filter_map(|e| match e {
                FarmEvent::Video { phone, frame } => Some((*phone, frame.seq, frame.pts_us)),
                _ => None,
            })
            .collect();
        assert_eq!(vids, vec![(1, 1, 10), (1, 2, 11)]);

        // Кэш без потребления: last_frame тот же второй кадр.
        let last = d.last_frame(1).unwrap().expect("кэш");
        assert_eq!((last.seq, last.pts_us), (2, 11));
        // Вторая трубка своих кадров не видела, чужих не получила.
        assert!(d.last_frame(2).unwrap().is_none());

        // Команды разошлись по трубкам.
        thread::sleep(Duration::from_millis(100));
        let i1 = inputs1.lock().unwrap().clone().join("\n");
        let i2 = inputs2.lock().unwrap().clone().join("\n");
        assert!(i1.contains("Scroll"), "{i1}");
        assert!(i2.contains("Wake"), "{i2}");
        assert!(!i1.contains("Wake") && !i2.contains("Scroll"));
    }

    #[test]
    fn unwatched_video_not_forwarded_but_cached() {
        let mut d = Farm::new();
        let (l, _) = FakeLink::with_events(vec![hello(), video(5)]);
        d.add_phone(7, Box::new(l)).unwrap();
        // watching выключен: видео в событиях нет, но кэш есть.
        let got = wait_for(&mut d, 0, 500);
        assert!(!got.iter().any(|e| matches!(e, FarmEvent::Video { .. })));
        thread::sleep(Duration::from_millis(200));
        let got = d.drain();
        assert!(!got.iter().any(|e| matches!(e, FarmEvent::Video { .. })));
        let last = d.last_frame(7).unwrap().expect("кэш без просмотра");
        assert_eq!(last.pts_us, 5);
    }

    /// Источник с переключаемым списком фактов.
    struct FakeScan(Arc<Mutex<Vec<Sighting>>>);

    impl Discovery for FakeScan {
        fn scan(&mut self) -> Vec<Sighting> {
            self.0.lock().unwrap().clone()
        }
    }

    struct FakeFactory;

    impl LinkFactory for FakeFactory {
        fn make(&mut self, _: &Sighting) -> Result<Box<dyn PhoneLink>, String> {
            Ok(Box::new(FakeLink::with_events(vec![hello()]).0))
        }
    }

    fn sight(transport: Transport, address: &str, serial: Option<&str>) -> Sighting {
        Sighting { transport, address: address.into(), serial: serial.map(String::from), issue: None }
    }

    fn auto_farm(lost_after: Duration) -> (Farm, Arc<Mutex<Vec<Sighting>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut f = Farm::new();
        let cfg = DiscoveryConfig { every: Duration::ZERO, lost_after, ..Default::default() };
        f.enable_discovery(vec![Box::new(FakeScan(seen.clone()))], Box::new(FakeFactory), cfg);
        (f, seen)
    }

    fn appeared(evs: &[FarmEvent]) -> Vec<(PhoneId, Transport)> {
        evs.iter()
            .filter_map(|e| match e {
                FarmEvent::Appeared { phone, transport, .. } => Some((*phone, *transport)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn empty_farm_picks_up_phone_that_appears_later() {
        let (mut f, seen) = auto_farm(Duration::from_secs(3600));
        assert!(f.discover_now().is_empty());
        assert!(f.phones().is_empty());
        seen.lock().unwrap().push(sight(Transport::Usb, "1-2", Some("AAA")));
        let evs = f.discover_now();
        assert_eq!(appeared(&evs), vec![(1, Transport::Usb)]);
        assert_eq!(f.phones(), vec![1]);
        // Повторный скан того же телефона ничего не меняет.
        assert!(appeared(&f.discover_now()).is_empty());
        assert_eq!(f.phones(), vec![1]);
    }

    #[test]
    fn one_phone_on_two_transports_is_one_slot_and_prefers_usb() {
        let (mut f, seen) = auto_farm(Duration::from_secs(3600));
        *seen.lock().unwrap() =
            vec![sight(Transport::Adb, "AAA", Some("AAA")), sight(Transport::Usb, "1-2", Some("AAA"))];
        let evs = f.discover_now();
        assert_eq!(appeared(&evs), vec![(1, Transport::Usb)]);
        assert_eq!(f.phones().len(), 1);
    }

    #[test]
    fn phones_are_pinned_and_unpinned_at_runtime() {
        let (mut f, seen) = auto_farm(Duration::from_secs(3600));
        f.auto.as_mut().unwrap().cfg.rules.auto = false;
        *seen.lock().unwrap() =
            vec![sight(Transport::Usb, "1-2", Some("AAA")), sight(Transport::Adb, "BBB", Some("BBB"))];
        assert!(appeared(&f.discover_now()).is_empty(), "без закрепления ничего не берём");
        f.pin(PhoneSpec::parse("usb:1-2").unwrap()).unwrap();
        assert_eq!(appeared(&f.discover_now()), vec![(1, Transport::Usb)]);
        assert!(f.unpin(&PhoneSpec::parse("adb:BBB").unwrap()).is_err());
        assert_eq!(f.unpin(&PhoneSpec::parse("usb:1-2").unwrap()).unwrap(), vec![1]);
        assert!(f.phones().is_empty());
        assert!(f.drain().iter().any(|e| matches!(e, FarmEvent::Lost { phone: 1 })));
        assert!(appeared(&f.discover_now()).is_empty(), "снятый телефон не возвращается сам");
    }

    #[test]
    fn set_transport_moves_a_phone_to_another_path_and_reports_unavailable_ones() {
        let (mut f, seen) = auto_farm(Duration::from_secs(3600));
        *seen.lock().unwrap() =
            vec![sight(Transport::Usb, "1-2", Some("AAA")), sight(Transport::Adb, "AAA", Some("AAA"))];
        f.discover_now();
        assert_eq!(f.phones_info()[0].transport, Some(Transport::Usb));
        // Оба пути видны: переводим на ADB под тем же номером.
        f.set_transport(1, Transport::Adb).unwrap();
        let evs = f.discover_now();
        assert_eq!(appeared(&evs), vec![(1, Transport::Adb)]);
        assert_eq!(f.phones(), vec![1]);
        // Выбор запомнен: следующий обзор не возвращает на USB.
        assert!(appeared(&f.discover_now()).is_empty());
        // Пути serial у телефона нет: предупреждение, путь не меняется.
        f.set_transport(1, Transport::Serial).unwrap();
        let evs = f.discover_now();
        assert!(evs.iter().any(|e| matches!(e, FarmEvent::Warning { phone: 1, .. })));
        assert_eq!(f.phones_info()[0].transport, Some(Transport::Adb));
        assert!(f.set_transport(9, Transport::Usb).is_err());
    }

    #[test]
    fn switches_transport_when_current_path_disappears() {
        let (mut f, seen) = auto_farm(Duration::from_secs(3600));
        *seen.lock().unwrap() =
            vec![sight(Transport::Usb, "1-2", Some("AAA")), sight(Transport::Adb, "AAA", Some("AAA"))];
        f.discover_now();
        // USB-путь пропал (кабель переключили), ADB остался: тот же номер, другой путь.
        *seen.lock().unwrap() = vec![sight(Transport::Adb, "AAA", Some("AAA"))];
        let evs = f.discover_now();
        assert_eq!(appeared(&evs), vec![(1, Transport::Adb)]);
        assert_eq!(f.phones(), vec![1]);
    }

    #[test]
    fn short_absence_is_not_loss_and_long_one_is() {
        let (mut f, seen) = auto_farm(Duration::from_secs(3600));
        *seen.lock().unwrap() = vec![sight(Transport::Usb, "1-2", Some("AAA"))];
        f.discover_now();
        // Переход в режим аксессуара: телефон на секунды пропадает.
        seen.lock().unwrap().clear();
        let evs = f.discover_now();
        assert!(!evs.iter().any(|e| matches!(e, FarmEvent::Lost { .. })));
        assert_eq!(f.phones(), vec![1]);

        let (mut f, seen) = auto_farm(Duration::ZERO);
        *seen.lock().unwrap() = vec![sight(Transport::Usb, "1-2", Some("AAA"))];
        f.discover_now();
        seen.lock().unwrap().clear();
        let evs = f.discover_now();
        assert!(evs.iter().any(|e| matches!(e, FarmEvent::Lost { phone: 1 })));
        assert!(f.phones().is_empty());
        // Вернулся тот же телефон: прежний номер.
        *seen.lock().unwrap() = vec![sight(Transport::Usb, "1-3", Some("AAA"))];
        assert_eq!(appeared(&f.discover_now()), vec![(1, Transport::Usb)]);
    }

    #[test]
    fn two_phones_get_two_numbers_and_manual_ones_are_left_alone() {
        let (mut f, seen) = auto_farm(Duration::ZERO);
        f.add_phone(1, Box::new(FakeLink::with_events(vec![]).0)).unwrap();
        *seen.lock().unwrap() =
            vec![sight(Transport::Usb, "1-2", Some("AAA")), sight(Transport::Usb, "1-3", Some("BBB"))];
        let evs = f.discover_now();
        let mut got = appeared(&evs);
        got.sort();
        assert_eq!(got, vec![(2, Transport::Usb), (3, Transport::Usb)]);
        // Телефон 1 добавлен руками: обнаружение его не теряет.
        seen.lock().unwrap().clear();
        f.discover_now();
        assert_eq!(f.phones(), vec![1]);
    }

    #[test]
    fn state_follows_session_events() {
        let mut d = Farm::new();
        let (l, _) = FakeLink::with_events(vec![
            hello(),
            Event::Status(pcdock_core::packet::Status { code: 1, detail: "projection stopped".into() }),
            video(3),
        ]);
        d.add_phone(5, Box::new(l)).unwrap();
        assert_eq!(d.state(5).unwrap(), PhoneState::Connecting);
        let mut states = Vec::new();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(2) && states.len() < 3 {
            for e in d.drain() {
                if let FarmEvent::State { state, .. } = e {
                    states.push(state);
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(states, vec![PhoneState::Ready, PhoneState::NeedsPermission, PhoneState::Ready]);
    }

    #[test]
    fn phone_specs_parse_and_match() {
        let usb_port = PhoneSpec::parse("usb:1-2.3").unwrap();
        assert_eq!(usb_port.address.as_deref(), Some("1-2.3"));
        let usb_serial = PhoneSpec::parse("usb:ABC123").unwrap();
        assert_eq!(usb_serial.serial.as_deref(), Some("ABC123"));
        assert_eq!(
            PhoneSpec::parse("usb").unwrap(),
            PhoneSpec { transport: Some(Transport::Usb), ..Default::default() }
        );
        assert!(PhoneSpec::parse("adb").is_err());
        assert!(PhoneSpec::parse("usb:").is_err());
        assert!(PhoneSpec::parse("bt:1").is_err());

        assert!(usb_port.matches(&sight(Transport::Usb, "1-2.3", None)));
        assert!(!usb_port.matches(&sight(Transport::Usb, "1-4", None)));
        assert!(!usb_port.matches(&sight(Transport::Adb, "1-2.3", None)));
        assert!(usb_serial.matches(&sight(Transport::Usb, "1-9", Some("ABC123"))));
        assert!(!usb_serial.matches(&sight(Transport::Adb, "ABC123", Some("ABC123"))));
        assert!(PhoneSpec::parse("adb:ABC123").unwrap().matches(&sight(
            Transport::Adb,
            "ABC123",
            Some("ABC123")
        )));
    }

    #[test]
    fn rules_combine_auto_except_and_pinned() {
        let usb = sight(Transport::Usb, "1-2", Some("AAA"));
        let adb = sight(Transport::Adb, "BBB", Some("BBB"));
        let only_pinned =
            Rules { auto: false, pinned: vec![PhoneSpec::parse("adb:BBB").unwrap()], ..Rules::default() };
        assert!(!only_pinned.accepts(&usb));
        assert!(only_pinned.accepts(&adb));

        let auto_except = Rules { auto: true, except: vec!["usb".into(), "BBB".into()], pinned: vec![] };
        assert!(!auto_except.accepts(&usb) && !auto_except.accepts(&adb));

        // Закреплённый телефон принимается, даже если автообнаружение его исключило.
        let both = Rules {
            auto: true,
            except: vec!["usb".into()],
            pinned: vec![PhoneSpec::parse("usb:1-2").unwrap()],
        };
        assert!(both.accepts(&usb));
        assert!(Rules::default().accepts(&usb));
    }

    #[test]
    fn pinned_phone_is_not_taken_over_other_transports() {
        let rules = Rules { auto: true, except: vec![], pinned: vec![PhoneSpec::parse("adb:AAA").unwrap()] };
        assert!(rules.accepts(&sight(Transport::Adb, "AAA", Some("AAA"))));
        // Тот же телефон по USB: закреплён за ADB, поэтому не берём.
        assert!(!rules.accepts(&sight(Transport::Usb, "1-2", Some("AAA"))));
        // Другие телефоны по-прежнему берутся автообнаружением.
        assert!(rules.accepts(&sight(Transport::Usb, "1-3", Some("BBB"))));
    }

    #[test]
    fn pinned_only_farm_ignores_other_phones() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut f = Farm::new();
        let rules =
            Rules { auto: false, pinned: vec![PhoneSpec::parse("usb:1-3").unwrap()], ..Rules::default() };
        let cfg = DiscoveryConfig { rules, every: Duration::ZERO, ..Default::default() };
        f.enable_discovery(vec![Box::new(FakeScan(seen.clone()))], Box::new(FakeFactory), cfg);
        *seen.lock().unwrap() =
            vec![sight(Transport::Usb, "1-2", Some("AAA")), sight(Transport::Usb, "1-3", Some("BBB"))];
        assert_eq!(appeared(&f.discover_now()), vec![(1, Transport::Usb)]);
        assert_eq!(f.phones(), vec![1]);
    }

    fn issue(transport: Transport, address: &str, serial: &str, issue: Issue) -> Sighting {
        Sighting { issue: Some(issue), ..sight(transport, address, Some(serial)) }
    }

    fn states(evs: &[FarmEvent]) -> Vec<PhoneState> {
        evs.iter()
            .filter_map(|e| match e {
                FarmEvent::State { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn unauthorized_adb_phone_is_parked_with_a_hint_then_connected() {
        let (mut f, seen) = auto_farm(Duration::from_secs(3600));
        *seen.lock().unwrap() = vec![issue(Transport::Adb, "AAA", "AAA", Issue::Unauthorized)];
        let evs = f.discover_now();
        assert_eq!(appeared(&evs), vec![(1, Transport::Adb)]);
        assert_eq!(states(&evs), vec![PhoneState::NeedsAuth]);
        assert!(evs
            .iter()
            .any(|e| matches!(e, FarmEvent::State { detail, .. } if detail.contains("отладку"))));
        assert_eq!(f.state(1).unwrap(), PhoneState::NeedsAuth);
        // Повторный скан с той же помехой ничего не меняет.
        let again = f.discover_now();
        assert!(states(&again).is_empty());

        // Человек нажал «Разрешить»: тот же номер, настоящий канал.
        *seen.lock().unwrap() = vec![sight(Transport::Adb, "AAA", Some("AAA"))];
        f.discover_now();
        assert_eq!(f.phones(), vec![1]);
        let t0 = Instant::now();
        let mut ready = false;
        while t0.elapsed() < Duration::from_secs(2) && !ready {
            ready = f.drain().iter().any(|e| matches!(e, FarmEvent::State { state: PhoneState::Ready, .. }));
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready, "после снятия помехи телефон дошёл до ready");
    }

    #[test]
    fn usb_without_access_is_reported_and_recovers() {
        let (mut f, seen) = auto_farm(Duration::from_secs(3600));
        *seen.lock().unwrap() = vec![issue(Transport::Usb, "1-2", "AAA", Issue::NoAccess)];
        f.discover_now();
        assert_eq!(f.state(1).unwrap(), PhoneState::NeedsAccess);
        *seen.lock().unwrap() = vec![sight(Transport::Usb, "1-2", Some("AAA"))];
        f.discover_now();
        assert_eq!(f.phones(), vec![1]);
    }

    #[test]
    fn silent_phone_before_handshake_means_no_app() {
        let mut d = Farm::new();
        let (l, _) = FakeLink::with_events(vec![Event::PeerSilent]);
        d.add_phone(4, Box::new(l)).unwrap();
        let t0 = Instant::now();
        let mut hint = None;
        while t0.elapsed() < Duration::from_secs(2) && hint.is_none() {
            for e in d.drain() {
                if let FarmEvent::State { state: PhoneState::NeedsApp, detail, .. } = e {
                    hint = Some(detail);
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(hint.expect("needs_app").contains("приложение"));
    }

    struct BlockedLink;

    impl PhoneLink for BlockedLink {
        fn is_connected(&self) -> bool {
            false
        }
        fn connect(&mut self) -> Result<(), LinkError> {
            Err(LinkError::Blocked(PhoneState::NeedsAccess, "Access denied".into()))
        }
        fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
            Ok(vec![])
        }
        fn input(&mut self, _: &InputCmd) -> Result<(), LinkError> {
            Ok(())
        }
        fn shutdown(&mut self) {}
    }

    #[test]
    fn blocked_link_reports_state_once_not_every_retry() {
        let mut d = Farm::new();
        d.add_phone(2, Box::new(BlockedLink)).unwrap();
        thread::sleep(Duration::from_millis(1600));
        let evs = d.drain();
        let blocked = evs
            .iter()
            .filter(|e| matches!(e, FarmEvent::State { state: PhoneState::NeedsAccess, .. }))
            .count();
        let down = evs.iter().filter(|e| matches!(e, FarmEvent::LinkDown { .. })).count();
        assert_eq!((blocked, down), (1, 1), "причина названа один раз: {evs:?}");
        assert_eq!(d.state(2).unwrap(), PhoneState::NeedsAccess);
    }

    /// USB-канал молчит (приложения нет), ADB-канал отвечает.
    struct UsbSilentFactory;

    impl LinkFactory for UsbSilentFactory {
        fn make(&mut self, s: &Sighting) -> Result<Box<dyn PhoneLink>, String> {
            let events = if s.transport == Transport::Usb { vec![Event::PeerSilent] } else { vec![hello()] };
            Ok(Box::new(FakeLink::with_events(events).0))
        }
    }

    fn wait_state(f: &mut Farm, id: PhoneId, want: PhoneState) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            f.drain();
            if f.state(id) == Ok(want) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn fallback_farm(fallback: Option<Duration>) -> (Farm, Arc<Mutex<Vec<Sighting>>>) {
        let seen = Arc::new(Mutex::new(vec![
            sight(Transport::Usb, "1-2", Some("AAA")),
            sight(Transport::Adb, "AAA", Some("AAA")),
        ]));
        let mut f = Farm::new();
        let cfg = DiscoveryConfig {
            every: Duration::ZERO,
            lost_after: Duration::from_secs(3600),
            fallback_after: fallback,
            ..Default::default()
        };
        f.enable_discovery(vec![Box::new(FakeScan(seen.clone()))], Box::new(UsbSilentFactory), cfg);
        (f, seen)
    }

    #[test]
    fn silent_usb_phone_falls_back_to_adb_and_remembers_it() {
        let (mut f, seen) = fallback_farm(Some(Duration::ZERO));
        assert_eq!(appeared(&f.discover_now()), vec![(1, Transport::Usb)], "первым берём USB");
        assert!(wait_state(&mut f, 1, PhoneState::NeedsApp), "приложения нет");
        // Следующий проход: USB не стал рабочим, ADB есть — переходим на него под тем же номером.
        let evs = f.discover_now();
        assert!(appeared(&evs).contains(&(1, Transport::Adb)), "{evs:?}");
        assert!(wait_state(&mut f, 1, PhoneState::Ready), "по ADB телефон готов");
        f.discover_now(); // запоминаем, что ADB сработал
                          // Телефон пропал и вернулся: теперь сразу ADB, а не USB.
        *seen.lock().unwrap() = vec![];
        let mut f2 = f;
        if let Some(a) = f2.auto.as_mut() {
            a.cfg.lost_after = Duration::ZERO;
        }
        f2.discover_now();
        assert!(f2.phones().is_empty());
        *seen.lock().unwrap() =
            vec![sight(Transport::Usb, "1-2", Some("AAA")), sight(Transport::Adb, "AAA", Some("AAA"))];
        assert_eq!(appeared(&f2.discover_now()), vec![(1, Transport::Adb)], "вспомнили рабочий путь");
    }

    #[test]
    fn no_fallback_when_disabled_or_nothing_to_fall_back_to() {
        let (mut f, _) = fallback_farm(None);
        f.discover_now();
        assert!(wait_state(&mut f, 1, PhoneState::NeedsApp));
        assert!(appeared(&f.discover_now()).is_empty(), "переход выключен");

        let seen = Arc::new(Mutex::new(vec![sight(Transport::Usb, "1-2", Some("AAA"))]));
        let mut g = Farm::new();
        let cfg = DiscoveryConfig {
            every: Duration::ZERO,
            fallback_after: Some(Duration::ZERO),
            ..Default::default()
        };
        g.enable_discovery(vec![Box::new(FakeScan(seen))], Box::new(UsbSilentFactory), cfg);
        g.discover_now();
        assert!(wait_state(&mut g, 1, PhoneState::NeedsApp));
        assert!(appeared(&g.discover_now()).is_empty(), "другого пути нет");
    }

    #[test]
    fn per_phone_preference_beats_general_order() {
        let seen = Arc::new(Mutex::new(vec![
            sight(Transport::Usb, "1-2", Some("AAA")),
            sight(Transport::Adb, "AAA", Some("AAA")),
            sight(Transport::Usb, "1-3", Some("BBB")),
            sight(Transport::Adb, "BBB", Some("BBB")),
        ]));
        let mut f = Farm::new();
        let mut prefer = std::collections::HashMap::new();
        prefer.insert("AAA".to_string(), Transport::Adb);
        let cfg = DiscoveryConfig { every: Duration::ZERO, prefer, ..Default::default() };
        f.enable_discovery(vec![Box::new(FakeScan(seen))], Box::new(FakeFactory), cfg);
        let mut got = appeared(&f.discover_now());
        got.sort();
        // AAA — по ADB (по настройке), BBB — по USB (общий порядок).
        let transports: Vec<Transport> = got.iter().map(|(_, t)| *t).collect();
        assert!(transports.contains(&Transport::Adb) && transports.contains(&Transport::Usb), "{got:?}");
        assert_eq!(f.phones().len(), 2);
    }

    /// Источник, у которого признак изменения выставляет тест.
    struct EventScan {
        seen: Arc<Mutex<Vec<Sighting>>>,
        changed: Arc<std::sync::atomic::AtomicBool>,
        scans: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Discovery for EventScan {
        fn scan(&mut self) -> Vec<Sighting> {
            self.scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.seen.lock().unwrap().clone()
        }
        fn changed(&mut self) -> bool {
            self.changed.swap(false, std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn hotplug_event_triggers_a_scan_before_the_interval() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let changed = Arc::new(AtomicBool::new(false));
        let scans = Arc::new(AtomicUsize::new(0));
        let mut f = Farm::new();
        let cfg = DiscoveryConfig { every: Duration::from_secs(3600), ..Default::default() };
        let src = EventScan { seen: seen.clone(), changed: changed.clone(), scans: scans.clone() };
        f.enable_discovery(vec![Box::new(src)], Box::new(FakeFactory), cfg);
        f.drain(); // первый проход идёт сразу
        assert_eq!(scans.load(Ordering::SeqCst), 1);
        f.drain();
        assert_eq!(scans.load(Ordering::SeqCst), 1, "интервал не прошёл, событий нет");
        seen.lock().unwrap().push(sight(Transport::Usb, "1-2", Some("AAA")));
        changed.store(true, Ordering::SeqCst);
        let evs = f.drain();
        assert_eq!(scans.load(Ordering::SeqCst), 2, "событие вызвало опрос");
        assert_eq!(appeared(&evs), vec![(1, Transport::Usb)]);
    }

    #[test]
    fn hello_packet_flows_through_link_trait() {
        // Связка пакетного уровня с типами демона: настоящий Hello из ядра.
        let hb = Hello {
            version_max: 2,
            version_min: 2,
            capabilities: 0,
            role: Role::Device,
            name: "phone".into(),
        }
        .to_packet()
        .encode();
        assert!(!hb.is_empty());
    }
}
