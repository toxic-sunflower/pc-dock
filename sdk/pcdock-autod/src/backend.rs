//! Что движку нужно от фермы: сведения о телефонах, ввод и кадры. Настоящая реализация (`FarmBackend`) говорит
//! с `pcdockd` по протоколу; в тестах подставляется фейк.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use pcdock_client::{Client, VideoFrame, KIND_FILE, KIND_VIDEO};
use serde_json::{json, Value};

/// Сведения о телефоне.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhoneInfo {
    pub id: u32,
    pub state: String,
    pub transport: Option<String>,
    pub address: Option<String>,
    pub serial: Option<String>,
    pub name: Option<String>,
    /// Ширина и высота экрана (по картинке), поворот.
    pub screen: Option<(u32, u32)>,
    pub rotation: u32,
}

/// Кадр экрана.
#[derive(Clone, Debug)]
pub struct Frame {
    pub seq: u64,
    pub pts_us: u64,
    pub jpeg: Arc<Vec<u8>>,
    /// Хэш содержимого для «картинка изменилась».
    pub hash: u64,
}

pub fn frame_hash(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    h
}

/// Фаза касания: 0 down, 1 move, 2 up, 3 cancel (как в протоколе демона).
pub type Phase = u32;

pub trait Backend: Send + Sync {
    /// Все телефоны фермы.
    fn phones(&self) -> Vec<PhoneInfo>;
    fn phone(&self, id: u32) -> Option<PhoneInfo> {
        self.phones().into_iter().find(|p| p.id == id)
    }
    #[allow(clippy::too_many_arguments)]
    fn touch(
        &self,
        phone: u32,
        phase: Phase,
        x: f32,
        y: f32,
        pressure: Option<f32>,
        size: Option<f32>,
        finger: u8,
    ) -> Result<(), String>;
    fn key(&self, phone: u32, usage: u16, down: bool) -> Result<(), String>;
    fn scroll(&self, phone: u32, ticks: f32, horizontal: bool) -> Result<(), String>;
    fn wake(&self, phone: u32) -> Result<(), String>;
    fn enable(&self, phone: u32, kind: u32, on: bool) -> Result<(), String>;
    /// Занимает телефон под кадры: до `release` ферма присылает кадры (подписка с условиями).
    fn acquire(&self, phone: u32) -> Result<(), String>;
    fn release(&self, phone: u32);
    /// Последний полученный кадр, без ожидания.
    fn latest_frame(&self, phone: u32) -> Option<Frame>;
    /// Ждёт кадр: самый свежий, но не старше номера `after` (если задан), не дольше `timeout`.
    fn wait_frame(&self, phone: u32, after: Option<u64>, timeout: Duration) -> Option<Frame>;

    /// Записывает файл на телефон. `media` — сообщить галерее.
    fn file_put(&self, _phone: u32, _dest: &str, _data: &[u8], _media: bool) -> Result<(), String> {
        Err("передача файлов не поддерживается".into())
    }
    /// Читает файл с телефона целиком, не больше `max` байт.
    fn file_get(&self, _phone: u32, _src: &str, _max: usize) -> Result<Vec<u8>, String> {
        Err("передача файлов не поддерживается".into())
    }
    fn file_list(&self, _phone: u32, _path: &str) -> Result<Vec<String>, String> {
        Err("передача файлов не поддерживается".into())
    }
    /// Запрос к приложению на телефоне: `req` — `{"op": "LaunchApp" | "ListApps" | "UiDump" | "ExitInfo" | "IpCheck" | "HttpLocal" | "TypeText", …}`
    /// без `phone` и `request`. Возвращает ответ телефона без номера запроса (части `UiDump` склеены).
    fn app(&self, _phone: u32, _req: Value, _timeout: Duration) -> Result<String, String> {
        Err("запросы к приложению не поддерживаются".into())
    }
    /// Пакет приложения на переднем плане, если телефон его сообщает (возможность 6).
    fn foreground(&self, _phone: u32) -> Option<String> {
        None
    }
    fn file_delete(&self, _phone: u32, _path: &str) -> Result<(), String> {
        Err("передача файлов не поддерживается".into())
    }
}

/// Приёмник данных и итога передачи файла: данные и событие `FileDone`/`FileFailed` могут прийти в любом порядке.
#[derive(Default)]
struct Route {
    data: Vec<u8>,
    done: Option<Result<(), String>>,
}

#[derive(Default)]
struct Routes {
    map: Mutex<HashMap<u32, Route>>,
    changed: Condvar,
}

// ---------------------------------------------------------------------------------------------

/// Ответ на запрос к приложению: полученные части (у `UiDump` их несколько) и склеенный ответ.
type AppReply = (Vec<Option<String>>, Option<String>);

/// Ответы приложения на запросы (`Status` 18–25) и передний план (16/17).
#[derive(Default)]
struct AppReplies {
    /// Номер запроса → части ответа (у `UiDump` их несколько) и склеенный ответ.
    map: Mutex<HashMap<u32, AppReply>>,
    foreground: Mutex<HashMap<u32, String>>,
    changed: Condvar,
}

impl AppReplies {
    /// Разбирает `Status` телефона `phone`.
    fn on_status(&self, phone: u32, code: u64, detail: &str) {
        match code {
            16 => {
                self.foreground.lock().unwrap().insert(phone, detail.trim().to_string());
                return;
            }
            17 => {
                self.foreground.lock().unwrap().remove(&phone);
                return;
            }
            18..=20 | 22..=25 => {}
            _ => return,
        }
        let Some((id, rest)) = detail.split_once(' ') else { return };
        let Ok(id) = id.parse::<u32>() else { return };
        let mut map = self.map.lock().unwrap();
        let Some(entry) = map.get_mut(&id) else { return };
        if code == 20 {
            // "<i>/<n> <часть>"
            let (head, part) = rest.split_once(' ').unwrap_or((rest, ""));
            let Some((i, n)) = head.split_once('/') else { return };
            let (Ok(i), Ok(n)) = (i.parse::<usize>(), n.parse::<usize>()) else { return };
            if n == 0 || i == 0 || i > n || n > 10_000 {
                return;
            }
            entry.0.resize(n, None);
            entry.0[i - 1] = Some(part.to_string());
            if entry.0.iter().all(Option::is_some) {
                entry.1 = Some(entry.0.iter().flatten().cloned().collect());
            }
        } else {
            entry.1 = Some(rest.to_string());
        }
        self.changed.notify_all();
    }
}

struct FrameStore {
    frames: Mutex<HashMap<u32, Frame>>,
    changed: Condvar,
}

/// Реализация поверх клиента `pcdockd`.
pub struct FarmBackend {
    client: Client,
    phones: Arc<Mutex<HashMap<u32, PhoneInfo>>>,
    frames: Arc<FrameStore>,
    watches: Mutex<HashMap<u32, usize>>,
    /// Частота кадров, которую автоматизация просит у телефона.
    fps: f32,
    alive: Arc<AtomicBool>,
    routes: Arc<Routes>,
    apps: Arc<AppReplies>,
    next_request: AtomicU32,
}

fn info_from(v: &Value) -> Option<PhoneInfo> {
    let d = &v["display"];
    Some(PhoneInfo {
        id: v["phone"].as_u64()? as u32,
        state: v["state"].as_str().unwrap_or("").to_string(),
        transport: v["transport"].as_str().map(String::from),
        address: v["address"].as_str().map(String::from),
        serial: v["serial"].as_str().map(String::from),
        name: v["name"].as_str().map(String::from),
        screen: match (d["screen_w"].as_u64(), d["screen_h"].as_u64()) {
            (Some(w), Some(h)) => Some((w as u32, h as u32)),
            _ => None,
        },
        rotation: d["rotation"].as_u64().unwrap_or(0) as u32,
    })
}

impl FarmBackend {
    /// Подключается к `pcdockd` и начинает следить за телефонами. `fps` — частота кадров, которую просит автоматизация.
    pub fn new(client: Client, fps: f32) -> Result<Arc<Self>, String> {
        let phones: Arc<Mutex<HashMap<u32, PhoneInfo>>> = Arc::new(Mutex::new(HashMap::new()));
        let frames = Arc::new(FrameStore { frames: Mutex::new(HashMap::new()), changed: Condvar::new() });
        let alive = Arc::new(AtomicBool::new(true));
        let events = client.take_events().ok_or("события уже забраны")?;
        let binary = client.take_binary().ok_or("бинарные пакеты уже забраны")?;
        let routes = Arc::new(Routes::default());
        let apps = Arc::new(AppReplies::default());
        let me = Arc::new(Self {
            client: client.clone(),
            phones: phones.clone(),
            frames: frames.clone(),
            watches: Mutex::new(HashMap::new()),
            fps,
            alive: alive.clone(),
            routes: routes.clone(),
            apps: apps.clone(),
            next_request: AtomicU32::new(1),
        });
        me.refresh()?;

        // События о телефонах: обновляют состояние.
        {
            let (phones, alive, routes, apps) = (phones.clone(), alive.clone(), routes.clone(), apps.clone());
            thread::Builder::new()
                .name("autod-farm-events".into())
                .spawn(move || {
                    while let Ok(v) = events.recv() {
                        if let (Some(t), Some(ev)) = (v["transfer"].as_u64(), v["ev"].as_str()) {
                            let result = match ev {
                                "FileDone" => Some(Ok(())),
                                "FileFailed" => Some(Err(v["detail"].as_str().unwrap_or("").to_string())),
                                _ => None,
                            };
                            if let Some(r) = result {
                                routes.map.lock().unwrap().entry(t as u32).or_default().done = Some(r);
                                routes.changed.notify_all();
                            }
                            continue;
                        }
                        let id = v["phone"].as_u64().map(|x| x as u32);
                        let Some(id) = id else { continue };
                        if v["ev"] == "Status" {
                            apps.on_status(
                                id,
                                v["code"].as_u64().unwrap_or(0),
                                v["detail"].as_str().unwrap_or(""),
                            );
                            continue;
                        }
                        let mut map = phones.lock().unwrap();
                        match v["ev"].as_str() {
                            Some("Appeared") => {
                                let e =
                                    map.entry(id).or_insert_with(|| PhoneInfo { id, ..Default::default() });
                                e.transport = v["transport"].as_str().map(String::from);
                                e.address = v["address"].as_str().map(String::from);
                                e.serial = v["serial"].as_str().map(String::from);
                            }
                            Some("State") => {
                                let e =
                                    map.entry(id).or_insert_with(|| PhoneInfo { id, ..Default::default() });
                                e.state = v["state"].as_str().unwrap_or("").to_string();
                            }
                            Some("Lost") => {
                                map.remove(&id);
                            }
                            Some("Display") => {
                                let e =
                                    map.entry(id).or_insert_with(|| PhoneInfo { id, ..Default::default() });
                                if let (Some(w), Some(h)) = (v["screen_w"].as_u64(), v["screen_h"].as_u64()) {
                                    e.screen = Some((w as u32, h as u32));
                                }
                                e.rotation = v["rotation"].as_u64().unwrap_or(0) as u32;
                            }
                            _ => {}
                        }
                    }
                    alive.store(false, Ordering::SeqCst);
                })
                .map_err(|e| e.to_string())?;
        }
        // Кадры: последний кадр каждого телефона.
        {
            let (frames, alive, routes) = (frames.clone(), alive.clone(), routes.clone());
            thread::Builder::new()
                .name("autod-farm-frames".into())
                .spawn(move || {
                    while let Ok((kind, payload)) = binary.recv() {
                        if kind == KIND_FILE && payload.len() >= 4 {
                            let id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                            routes
                                .map
                                .lock()
                                .unwrap()
                                .entry(id)
                                .or_default()
                                .data
                                .extend_from_slice(&payload[4..]);
                            routes.changed.notify_all();
                            continue;
                        }
                        if kind != KIND_VIDEO {
                            continue;
                        }
                        let Some(f) = VideoFrame::parse(&payload) else { continue };
                        let hash = frame_hash(&f.jpeg);
                        frames.frames.lock().unwrap().insert(
                            f.phone,
                            Frame { seq: f.seq, pts_us: f.pts_us, jpeg: Arc::new(f.jpeg), hash },
                        );
                        frames.changed.notify_all();
                    }
                    alive.store(false, Ordering::SeqCst);
                    frames.changed.notify_all();
                })
                .map_err(|e| e.to_string())?;
        }
        Ok(me)
    }

    /// Жива ли связь с `pcdockd`.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && !self.client.is_closed()
    }

    /// Перечитывает список телефонов со всеми подробностями.
    pub fn refresh(&self) -> Result<(), String> {
        let r = self.client.request(json!({"op": "List"})).map_err(|e| e.to_string())?;
        let mut map = self.phones.lock().unwrap();
        map.clear();
        for d in r["details"].as_array().into_iter().flatten() {
            if let Some(i) = info_from(d) {
                map.insert(i.id, i);
            }
        }
        Ok(())
    }

    /// Ждёт итога передачи `t`.
    fn wait_route(&self, t: u32, _ready: impl Fn(&Route) -> bool, timeout: Duration) -> Result<(), String> {
        let end = Instant::now() + timeout;
        let mut map = self.routes.map.lock().unwrap();
        loop {
            if let Some(done) = map.get(&t).and_then(|r| r.done.clone()) {
                return done;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("передача файла не завершилась вовремя".into());
            }
            if !self.alive.load(Ordering::SeqCst) {
                return Err("связь с pcdockd потеряна".into());
            }
            map = self.routes.changed.wait_timeout(map, left.min(Duration::from_millis(200))).unwrap().0;
        }
    }

    fn op(&self, msg: Value) -> Result<(), String> {
        self.client.request(msg).map(|_| ()).map_err(|e| e.to_string())
    }
}

impl Backend for FarmBackend {
    fn phones(&self) -> Vec<PhoneInfo> {
        let mut v: Vec<PhoneInfo> = self.phones.lock().unwrap().values().cloned().collect();
        v.sort_by_key(|p| p.id);
        v
    }

    fn touch(
        &self,
        phone: u32,
        phase: Phase,
        x: f32,
        y: f32,
        pressure: Option<f32>,
        size: Option<f32>,
        finger: u8,
    ) -> Result<(), String> {
        self.op(json!({"op": "TouchEx", "phone": phone, "cid": finger, "phase": phase, "x": x, "y": y,
                       "width": size, "height": size, "pressure": pressure}))
    }

    fn key(&self, phone: u32, usage: u16, down: bool) -> Result<(), String> {
        self.op(json!({"op": "Key", "phone": phone, "usage": usage, "down": down}))
    }

    fn scroll(&self, phone: u32, ticks: f32, horizontal: bool) -> Result<(), String> {
        self.op(json!({"op": if horizontal { "ScrollH" } else { "Scroll" }, "phone": phone, "ticks": ticks}))
    }

    fn wake(&self, phone: u32) -> Result<(), String> {
        self.op(json!({"op": "Wake", "phone": phone}))
    }

    fn enable(&self, phone: u32, kind: u32, on: bool) -> Result<(), String> {
        self.op(json!({"op": "DeviceEnable", "phone": phone, "kind": kind, "on": on}))
    }

    fn acquire(&self, phone: u32) -> Result<(), String> {
        let mut w = self.watches.lock().unwrap();
        let n = w.entry(phone).or_insert(0);
        if *n == 0 {
            // Подписка с умеренной частотой: автоматизация не требует плавного видео и не заставляет телефон его давать.
            self.op(json!({"op": "Watch", "phone": phone, "fps": self.fps}))?;
        }
        *n += 1;
        Ok(())
    }

    fn release(&self, phone: u32) {
        let mut w = self.watches.lock().unwrap();
        if let Some(n) = w.get_mut(&phone) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                w.remove(&phone);
                let _ = self.op(json!({"op": "Unwatch", "phone": phone}));
                self.frames.frames.lock().unwrap().remove(&phone);
            }
        }
    }

    fn latest_frame(&self, phone: u32) -> Option<Frame> {
        self.frames.frames.lock().unwrap().get(&phone).cloned()
    }

    fn file_put(&self, phone: u32, dest: &str, data: &[u8], media: bool) -> Result<(), String> {
        let r = self
            .client
            .request(
                json!({"op": "FilePut", "phone": phone, "dest": dest, "size": data.len(), "media": media}),
            )
            .map_err(|e| e.to_string())?;
        let t = r["transfer"].as_u64().ok_or("нет номера передачи")? as u32;
        for part in data.chunks(60_000) {
            let mut p = t.to_le_bytes().to_vec();
            p.extend_from_slice(part);
            self.client.send_binary(KIND_FILE, &p).map_err(|e| e.to_string())?;
        }
        let res = self.wait_route(t, |_| true, Duration::from_secs(120));
        self.routes.map.lock().unwrap().remove(&t);
        res.map(|_| ())
    }

    fn file_get(&self, phone: u32, src: &str, max: usize) -> Result<Vec<u8>, String> {
        let r = self
            .client
            .request(json!({"op": "FileGet", "phone": phone, "src": src}))
            .map_err(|e| e.to_string())?;
        let t = r["transfer"].as_u64().ok_or("нет номера передачи")? as u32;
        let size = r["size"].as_u64().unwrap_or(0) as usize;
        if size > max {
            let _ = self.client.request(json!({"op": "FileCancel", "transfer": t}));
            self.routes.map.lock().unwrap().remove(&t);
            return Err(format!("файл {size} байт больше предела {max}"));
        }
        let res = self.wait_route(t, |_| true, Duration::from_secs(120));
        // Итог (`FileDone`) и данные приходят разными потоками клиента и могут разойтись по времени:
        // после итога дожидаемся последних кусков.
        if res.is_ok() {
            let end = Instant::now() + Duration::from_secs(10);
            let mut map = self.routes.map.lock().unwrap();
            while map.get(&t).map_or(0, |r| r.data.len()) < size && Instant::now() < end {
                map = self.routes.changed.wait_timeout(map, Duration::from_millis(50)).unwrap().0;
            }
        }
        let data = self.routes.map.lock().unwrap().remove(&t).map(|r| r.data).unwrap_or_default();
        res?;
        if data.len() != size {
            return Err(format!("получено {} из {size} байт", data.len()));
        }
        Ok(data)
    }

    fn file_list(&self, phone: u32, path: &str) -> Result<Vec<String>, String> {
        let r = self
            .client
            .request(json!({"op": "FileList", "phone": phone, "path": path}))
            .map_err(|e| e.to_string())?;
        Ok(r["names"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect())
    }

    fn file_delete(&self, phone: u32, path: &str) -> Result<(), String> {
        self.op(json!({"op": "FileDelete", "phone": phone, "path": path}))
    }

    fn app(&self, phone: u32, mut req: Value, timeout: Duration) -> Result<String, String> {
        let id = self.next_request.fetch_add(1, Ordering::SeqCst);
        req["phone"] = json!(phone);
        req["request"] = json!(id);
        self.apps.map.lock().unwrap().insert(id, (Vec::new(), None));
        if let Err(e) = self.op(req) {
            self.apps.map.lock().unwrap().remove(&id);
            return Err(e);
        }
        let end = Instant::now() + timeout;
        let mut map = self.apps.map.lock().unwrap();
        loop {
            if let Some(done) = map.get(&id).and_then(|r| r.1.clone()) {
                map.remove(&id);
                return Ok(done);
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.alive.load(Ordering::SeqCst) {
                map.remove(&id);
                return Err("телефон не ответил вовремя".into());
            }
            map = self.apps.changed.wait_timeout(map, left.min(Duration::from_millis(200))).unwrap().0;
        }
    }

    fn foreground(&self, phone: u32) -> Option<String> {
        self.apps.foreground.lock().unwrap().get(&phone).cloned()
    }

    fn wait_frame(&self, phone: u32, after: Option<u64>, timeout: Duration) -> Option<Frame> {
        let deadline = Instant::now() + timeout;
        let mut map = self.frames.frames.lock().unwrap();
        loop {
            if let Some(f) = map.get(&phone) {
                if after.is_none_or(|a| f.seq > a) {
                    return Some(f.clone());
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.alive.load(Ordering::SeqCst) {
                return None;
            }
            map = self.frames.changed.wait_timeout(map, left.min(Duration::from_millis(200))).unwrap().0;
        }
    }
}

// ---------------------------------------------------------------------------------------------

/// Куда подключаться: `pcdockd` по TCP или по локальной трубе.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FarmTarget {
    Tcp(String),
    Local(String),
}

impl FarmTarget {
    /// `host:port`, `tcp://host:port`, `local:имя` или `pipe:имя`.
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(a) = s.strip_prefix("tcp://") {
            return Ok(Self::Tcp(a.to_string()));
        }
        if let Some(n) = s.strip_prefix("local:").or_else(|| s.strip_prefix("pipe:")) {
            return if n.is_empty() {
                Err("пустое имя трубы".into())
            } else {
                Ok(Self::Local(n.to_string()))
            };
        }
        if s.contains(':') {
            return Ok(Self::Tcp(s.to_string()));
        }
        Err(format!("{s:?}: нужно host:port, tcp://host:port или local:имя"))
    }

    fn connect(&self, token: Option<&str>) -> Result<Client, String> {
        match self {
            Self::Tcp(a) => Client::connect_tcp(a, token),
            Self::Local(n) => Client::connect_local(n, token),
        }
        .map_err(|e| e.to_string())
    }
}

/// Связь с `pcdockd`, которая переподключается сама: пока связи нет, телефонов не видно и команды отказывают.
pub struct ReconnectBackend {
    inner: std::sync::RwLock<Option<Arc<FarmBackend>>>,
}

impl ReconnectBackend {
    /// Запускает поток подключения и переподключения.
    pub fn start(target: FarmTarget, token: Option<String>, fps: f32) -> Arc<Self> {
        let me = Arc::new(Self { inner: std::sync::RwLock::new(None) });
        let weak = Arc::downgrade(&me);
        thread::Builder::new()
            .name("autod-farm-link".into())
            .spawn(move || {
                let mut warned = false;
                loop {
                    let Some(me) = weak.upgrade() else { return };
                    let alive = me.inner.read().unwrap().as_ref().is_some_and(|b| b.is_alive());
                    if !alive {
                        if me.inner.read().unwrap().is_some() {
                            log::warn!("связь с pcdockd потеряна, переподключаюсь");
                            *me.inner.write().unwrap() = None;
                        }
                        match target.connect(token.as_deref()).and_then(|c| FarmBackend::new(c, fps)) {
                            Ok(b) => {
                                log::info!("подключён к pcdockd ({target:?})");
                                *me.inner.write().unwrap() = Some(b);
                                warned = false;
                            }
                            Err(e) => {
                                if !warned {
                                    log::warn!("pcdockd недоступен ({e}); повторяю");
                                    warned = true;
                                }
                            }
                        }
                    }
                    drop(me);
                    thread::sleep(Duration::from_secs(2));
                }
            })
            .expect("не встал поток связи с pcdockd");
        me
    }

    pub fn is_connected(&self) -> bool {
        self.inner.read().unwrap().as_ref().is_some_and(|b| b.is_alive())
    }

    fn get(&self) -> Result<Arc<FarmBackend>, String> {
        self.inner.read().unwrap().clone().ok_or_else(|| "нет связи с pcdockd".to_string())
    }
}

impl Backend for ReconnectBackend {
    fn phones(&self) -> Vec<PhoneInfo> {
        self.get().map(|b| b.phones()).unwrap_or_default()
    }
    fn touch(
        &self,
        phone: u32,
        phase: Phase,
        x: f32,
        y: f32,
        pressure: Option<f32>,
        size: Option<f32>,
        finger: u8,
    ) -> Result<(), String> {
        self.get()?.touch(phone, phase, x, y, pressure, size, finger)
    }
    fn key(&self, phone: u32, usage: u16, down: bool) -> Result<(), String> {
        self.get()?.key(phone, usage, down)
    }
    fn scroll(&self, phone: u32, ticks: f32, horizontal: bool) -> Result<(), String> {
        self.get()?.scroll(phone, ticks, horizontal)
    }
    fn wake(&self, phone: u32) -> Result<(), String> {
        self.get()?.wake(phone)
    }
    fn enable(&self, phone: u32, kind: u32, on: bool) -> Result<(), String> {
        self.get()?.enable(phone, kind, on)
    }
    fn acquire(&self, phone: u32) -> Result<(), String> {
        self.get()?.acquire(phone)
    }
    fn release(&self, phone: u32) {
        if let Ok(b) = self.get() {
            b.release(phone)
        }
    }
    fn latest_frame(&self, phone: u32) -> Option<Frame> {
        self.get().ok()?.latest_frame(phone)
    }
    fn wait_frame(&self, phone: u32, after: Option<u64>, timeout: Duration) -> Option<Frame> {
        self.get().ok()?.wait_frame(phone, after, timeout)
    }
    fn file_put(&self, phone: u32, dest: &str, data: &[u8], media: bool) -> Result<(), String> {
        self.get()?.file_put(phone, dest, data, media)
    }
    fn file_get(&self, phone: u32, src: &str, max: usize) -> Result<Vec<u8>, String> {
        self.get()?.file_get(phone, src, max)
    }
    fn file_list(&self, phone: u32, path: &str) -> Result<Vec<String>, String> {
        self.get()?.file_list(phone, path)
    }
    fn file_delete(&self, phone: u32, path: &str) -> Result<(), String> {
        self.get()?.file_delete(phone, path)
    }
    fn app(&self, phone: u32, req: Value, timeout: Duration) -> Result<String, String> {
        self.get()?.app(phone, req, timeout)
    }
    fn foreground(&self, phone: u32) -> Option<String> {
        self.get().ok()?.foreground(phone)
    }
}
