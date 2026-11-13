//! Сервер фермы: протокол приложения поверх общей основы демонов (`pcdock-net`).
//!
//! Один протокол везде (см. `protocol`): JSON-команды туда, JSON-события и бинарные кадры оттуда.
//!
//! Устройство раздачи:
//! - подписка на телефон несёт условия (частота, только при изменении, звук); кадры отбираются для каждого клиента
//!   отдельно, видео на телефоне идёт по спросу: нет подписчиков — выключено, иначе с частотой самого
//!   требовательного;
//! - у каждого клиента свой ящик (см. `pcdock_net::Outbox`): события не теряются, видео хранится одним слотом
//!   «последний кадр», звук идёт очередью с потерями; клиент, не читающий события, отключается и не может
//!   задержать остальных.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use pcdock_core::input::{DeviceKind, MouseButton, TouchPhase};
use pcdock_core::link::{AppRequest, InputCmd, PhoneId, PhoneState};
use pcdock_farm::{Farm, FarmEvent};
use pcdock_net::{Handler, Hub};
use serde_json::{json, Value};

use super::files::{FileOptions, MAX_FILE};
use super::protocol::{encode_audio, encode_video, ClientMsg, ServerMsg, PKG_AUDIO, PKG_FILE, PKG_VIDEO};

/// Версия протокола приложения (приходит клиенту в `Hello`).
pub const PROTOCOL_VERSION: u32 = 1;

/// Условия подписки клиента на телефон.
struct Watch {
    /// Не чаще стольких кадров в секунду; `None` — все кадры.
    max_fps: Option<f32>,
    on_change: bool,
    audio: bool,
    last_sent: Option<Instant>,
    last_hash: Option<u64>,
}

/// Передача файла клиента.
struct Transfer {
    /// Куда клиент шлёт данные (для записи на телефон).
    tx: Option<SyncSender<Vec<u8>>>,
    cancel: Arc<AtomicBool>,
}

/// Данные клиента фермы: его подписки и передачи файлов.
#[derive(Default)]
pub struct ClientState {
    watches: HashMap<PhoneId, Watch>,
    /// Телефоны, у которых клиент спрашивал приложение (`LaunchApp`…): `Status` от них идёт ему и без `Watch`.
    apps: std::collections::HashSet<PhoneId>,
    transfers: HashMap<u32, Transfer>,
}

/// Что сейчас запрошено у телефона: видео, звук и частота (0 — без ограничения).
type Demand = (bool, bool, u8);

/// Общее состояние сервера: ферма и то, что от неё запрошено.
struct Shared {
    daemon: Arc<Mutex<Farm>>,
    demand: Mutex<HashMap<PhoneId, Demand>>,
    files: Option<FileOptions>,
    next_transfer: AtomicU32,
}

/// Сервер поверх фермы. Потокобезопасен через `Arc`: слушатели и клиенты живут в своих потоках,
/// ферма дёргается под мьютексом.
#[derive(Clone)]
pub struct Serve {
    shared: Arc<Shared>,
    hub: Hub<ClientState>,
}

/// Хэш кадра для условия `on_change` (FNV-1a).
fn frame_hash(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    h
}

fn to_value(msg: &ServerMsg) -> Value {
    serde_json::to_value(msg).unwrap_or_else(|_| json!({"ev": "Error", "detail": "encode"}))
}

impl Serve {
    pub fn new(daemon: Farm) -> Self {
        Self::with_token(daemon, None)
    }

    /// С токеном: клиент первым сообщением шлёт `{"op":"Auth","token":"…"}`, до этого ему не отвечают
    /// и событий не присылают.
    pub fn with_token(daemon: Farm, token: Option<String>) -> Self {
        Self::with_options(daemon, token, None)
    }

    /// С паролем и передачей файлов (`files`: `None` — файлы выключены).
    pub fn with_options(daemon: Farm, token: Option<String>, files: Option<FileOptions>) -> Self {
        let shared = Arc::new(Shared {
            daemon: Arc::new(Mutex::new(daemon)),
            demand: Mutex::new(HashMap::new()),
            files,
            next_transfer: AtomicU32::new(1),
        });
        let hub = Hub::new(Handle { shared: shared.clone() }, token);
        let serve = Self { shared, hub };
        serve.spawn_pump();
        serve
    }

    /// Доступ к ферме (добавить трубку, положить команду напрямую).
    pub fn daemon(&self) -> &Arc<Mutex<Farm>> {
        &self.shared.daemon
    }

    pub fn run_tcp(&self, addr: &str) -> std::io::Result<std::net::SocketAddr> {
        self.hub.run_tcp(addr)
    }

    pub fn run_ws(&self, addr: &str) -> std::io::Result<std::net::SocketAddr> {
        self.hub.run_ws(addr)
    }

    pub fn run_local(&self, name: &str) -> std::io::Result<()> {
        self.hub.run_local(name)
    }

    /// Поток раздачи: забирает события фермы и рассылает их по подпискам.
    fn spawn_pump(&self) {
        let (shared, hub) = (self.shared.clone(), self.hub.clone());
        thread::Builder::new()
            .name("serve-pump".into())
            .spawn(move || loop {
                let events = shared.daemon.lock().unwrap().drain();
                for ev in events {
                    if let FarmEvent::Connected { phone, .. } = &ev {
                        // Телефон (пере)подключился и снова не знает, что от него нужно.
                        shared.refresh_demand(&hub, *phone, true);
                    }
                    route(&hub, ev);
                }
                thread::sleep(Duration::from_millis(5));
            })
            .expect("не встал поток раздач");
    }
}

impl Shared {
    /// Пересчитывает, что нужно телефону, по подпискам всех клиентов и сообщает ему, если изменилось
    /// (или если `force`: телефон только что подключился и ничего не знает).
    fn refresh_demand(&self, hub: &Hub<ClientState>, phone: PhoneId, force: bool) {
        let (video, audio, fps) = hub.with_clients(|clients| {
            let mut video = false;
            let mut audio = false;
            let mut unlimited = false;
            let mut max_fps = 0.0f32;
            for c in clients.values() {
                if let Some(w) = c.data.watches.get(&phone) {
                    video = true;
                    audio |= w.audio;
                    match w.max_fps {
                        Some(f) => max_fps = max_fps.max(f),
                        None => unlimited = true,
                    }
                }
            }
            let fps = if !video || unlimited { 0 } else { max_fps.ceil().clamp(1.0, 255.0) as u8 };
            (video, audio, fps)
        });
        {
            let mut demand = self.demand.lock().unwrap();
            let now = (video, audio, fps);
            if !force && demand.get(&phone) == Some(&now) {
                return;
            }
            demand.insert(phone, now);
        }
        let mut daemon = self.daemon.lock().unwrap();
        let _ = daemon.set_watching(phone, video);
        let _ = daemon.command(phone, InputCmd::SetStream { video, audio, max_fps: fps });
    }

    /// Последний кадр телефона из кэша — одному клиенту. `false`, если кадра нет.
    fn send_snapshot(&self, hub: &Hub<ClientState>, client: u64, phone: PhoneId) -> bool {
        let frame = self.daemon.lock().unwrap().last_frame(phone).ok().flatten();
        let Some(f) = frame else { return false };
        let payload = encode_video(phone, f.seq, f.codec, f.key_frame, f.pts_us, &f.data);
        hub.with_clients(|clients| {
            if let Some(c) = clients.get(&client) {
                c.out.latest(PKG_VIDEO, payload);
            }
        });
        true
    }
}

struct Handle {
    shared: Arc<Shared>,
}

impl Handler<ClientState> for Handle {
    fn client_data(&self) -> ClientState {
        ClientState::default()
    }

    fn hello(&self) -> Value {
        to_value(&ServerMsg::Hello {
            protocol: PROTOCOL_VERSION,
            server: format!("pcdockd/{}", env!("CARGO_PKG_VERSION")),
        })
    }

    fn on_binary(&self, _hub: &Hub<ClientState>, client: u64, kind: u8, payload: Vec<u8>) {
        if kind != PKG_FILE || payload.len() < 4 {
            return;
        }
        let id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let tx = _hub.with_clients(|m| {
            m.get(&client).and_then(|c| c.data.transfers.get(&id)).and_then(|t| t.tx.clone())
        });
        // Отправка вне замка: если запись на телефон медленная, чтение клиента ждёт, но остальные клиенты работают.
        if let Some(tx) = tx {
            let _ = tx.send(payload[4..].to_vec());
        }
    }

    fn on_close(&self, hub: &Hub<ClientState>, _client: u64, data: ClientState) {
        for t in data.transfers.values() {
            t.cancel.store(true, Ordering::SeqCst);
        }
        // Подписки ушедшего клиента снимаются: если смотреть больше некому, видео на телефоне выключается.
        for phone in data.watches.keys() {
            self.shared.refresh_demand(hub, *phone, false);
        }
    }

    fn on_json(&self, hub: &Hub<ClientState>, client: u64, id: Option<u64>, msg: Value) {
        let msg: ClientMsg = match serde_json::from_value(msg) {
            Ok(m) => m,
            Err(e) => return hub.error(client, id, format!("плохая команда: {e}")),
        };
        let reply = |m: ServerMsg| hub.reply(client, id, to_value(&m));
        let fail = |detail: String| hub.error(client, id, detail);
        match msg {
            ClientMsg::Auth { .. } => reply(ServerMsg::Authed),
            ClientMsg::List => {
                let info = self.shared.daemon.lock().unwrap().phones_info();
                let phones = info.iter().map(|i| i.id).collect();
                let details = info.iter().map(phone_json).collect();
                reply(ServerMsg::Phones { phones, details })
            }
            ClientMsg::Watch { phone, fps, on_change, audio } => {
                if !self.shared.daemon.lock().unwrap().phones().contains(&phone) {
                    return fail(format!("нет трубки {phone}"));
                }
                if fps.is_some_and(|f| !(f > 0.0 && f.is_finite())) {
                    return fail("плохая команда: fps должен быть больше 0".into());
                }
                hub.with_clients(|clients| {
                    if let Some(c) = clients.get_mut(&client) {
                        c.data.watches.insert(
                            phone,
                            Watch { max_fps: fps, on_change, audio, last_sent: None, last_hash: None },
                        );
                    }
                });
                self.shared.refresh_demand(hub, phone, false);
                reply(ServerMsg::Watched { phone, on: true });
                // Новый подписчик сразу получает последний кадр, чтобы не ждать изменения экрана.
                self.shared.send_snapshot(hub, client, phone);
            }
            ClientMsg::Unwatch { phone } => {
                hub.with_clients(|clients| {
                    if let Some(c) = clients.get_mut(&client) {
                        c.data.watches.remove(&phone);
                    }
                });
                self.shared.refresh_demand(hub, phone, false);
                reply(ServerMsg::Watched { phone, on: false });
            }
            ClientMsg::Snapshot { phone } => {
                if !self.shared.send_snapshot(hub, client, phone) {
                    fail(format!("нет кадра у трубки {phone}"));
                } else if id.is_some() {
                    reply(ServerMsg::Ok);
                }
            }
            ClientMsg::FilePut { phone, dest, size, media } => {
                self.file_put(hub, client, id, phone, dest, size, media)
            }
            ClientMsg::FileGet { phone, src } => self.file_get(hub, client, id, phone, src),
            ClientMsg::FileList { phone, path } => match self.file_target(phone, &path) {
                Ok((files, serial)) => match files.backend.list(&serial, &path) {
                    Ok(names) => reply(ServerMsg::Files { phone, path, names }),
                    Err(e) => fail(e),
                },
                Err(e) => fail(e),
            },
            ClientMsg::FileDelete { phone, path } => match self.file_target(phone, &path) {
                Ok((files, serial)) => match files.backend.delete(&serial, &path) {
                    Ok(()) => reply(ServerMsg::Ok),
                    Err(e) => fail(e),
                },
                Err(e) => fail(e),
            },
            ClientMsg::FileCancel { transfer } => {
                let found = hub.with_clients(|m| {
                    m.get(&client)
                        .and_then(|c| c.data.transfers.get(&transfer))
                        .map(|t| t.cancel.store(true, Ordering::SeqCst))
                        .is_some()
                });
                if found {
                    reply(ServerMsg::Ok)
                } else {
                    fail(format!("нет передачи {transfer}"))
                }
            }
            ClientMsg::SetTransport { phone, transport } => {
                let t = match transport.as_str() {
                    "usb" => pcdock_core::link::Transport::Usb,
                    "adb" => pcdock_core::link::Transport::Adb,
                    "serial" => pcdock_core::link::Transport::Serial,
                    _ => return fail("transport: usb, adb или serial".into()),
                };
                match self.shared.daemon.lock().unwrap().set_transport(phone, t) {
                    Ok(()) => {
                        if id.is_some() {
                            reply(ServerMsg::Ok);
                        }
                    }
                    Err(e) => fail(e),
                }
            }
            ClientMsg::Pin { spec } => {
                let r = pcdock_farm::PhoneSpec::parse(&spec)
                    .and_then(|s| self.shared.daemon.lock().unwrap().pin(s));
                match r {
                    Ok(()) => {
                        if id.is_some() {
                            reply(ServerMsg::Ok);
                        }
                    }
                    Err(e) => fail(e),
                }
            }
            ClientMsg::Unpin { spec } => {
                let r = pcdock_farm::PhoneSpec::parse(&spec)
                    .and_then(|s| self.shared.daemon.lock().unwrap().unpin(&s));
                match r {
                    Ok(_) => {
                        if id.is_some() {
                            reply(ServerMsg::Ok);
                        }
                    }
                    Err(e) => fail(e),
                }
            }
            ClientMsg::AppEvents { phone, on } => {
                if !self.shared.daemon.lock().unwrap().phones().contains(&phone) {
                    return fail(format!("нет трубки {phone}"));
                }
                hub.with_clients(|clients| {
                    if let Some(c) = clients.get_mut(&client) {
                        if on {
                            c.data.apps.insert(phone);
                        } else {
                            c.data.apps.remove(&phone);
                        }
                    }
                });
                if id.is_some() {
                    reply(ServerMsg::Ok);
                }
            }
            other => {
                let (phone, cmd) = match map_input(other) {
                    Ok(v) => v,
                    Err(detail) => return fail(detail),
                };
                if matches!(cmd, InputCmd::App(_)) {
                    hub.with_clients(|clients| {
                        if let Some(c) = clients.get_mut(&client) {
                            c.data.apps.insert(phone);
                        }
                    });
                }
                let result = self.shared.daemon.lock().unwrap().command(phone, cmd);
                match result {
                    Ok(()) => {
                        if id.is_some() {
                            reply(ServerMsg::Ok);
                        }
                    }
                    Err(e) => fail(e),
                }
            }
        }
    }
}

impl Handle {
    /// Передача файлов включена, путь разрешён, у телефона известен серийник.
    fn file_target(&self, phone: PhoneId, path: &str) -> Result<(&FileOptions, String), String> {
        let files = self.shared.files.as_ref().ok_or("передача файлов выключена (pcdockd --files)")?;
        files.check(path)?;
        let serial = self
            .shared
            .daemon
            .lock()
            .unwrap()
            .phones_info()
            .into_iter()
            .find(|i| i.id == phone)
            .ok_or_else(|| format!("нет трубки {phone}"))?
            .serial
            .ok_or("у телефона неизвестен серийник: файлы недоступны")?;
        Ok((files, serial))
    }

    #[allow(clippy::too_many_arguments)]
    fn file_put(
        &self,
        hub: &Hub<ClientState>,
        client: u64,
        id: Option<u64>,
        phone: PhoneId,
        dest: String,
        size: u64,
        media: bool,
    ) {
        let fail = |detail: String| hub.error(client, id, detail);
        if size > MAX_FILE {
            return fail(format!("файл больше {MAX_FILE} байт"));
        }
        let (files, serial) = match self.file_target(phone, &dest) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        let writer = match files.backend.put(&serial, &dest) {
            Ok(w) => w,
            Err(e) => return fail(e),
        };
        let transfer = self.shared.next_transfer.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
        let cancel = Arc::new(AtomicBool::new(false));
        hub.with_clients(|m| {
            if let Some(c) = m.get_mut(&client) {
                c.data.transfers.insert(transfer, Transfer { tx: Some(tx), cancel: cancel.clone() });
            }
        });
        hub.reply(client, id, to_value(&ServerMsg::FileReady { transfer }));
        let hub = hub.clone();
        thread::Builder::new()
            .name(format!("file-put-{transfer}"))
            .spawn(move || {
                let result = pump_put(&hub, client, transfer, size, media, writer, rx, &cancel);
                hub.with_clients(|m| {
                    if let Some(c) = m.get_mut(&client) {
                        c.data.transfers.remove(&transfer);
                    }
                });
                let msg = match result {
                    Ok(()) => ServerMsg::FileDone { transfer, size },
                    Err(detail) => ServerMsg::FileFailed { transfer, detail },
                };
                hub.reply(client, None, to_value(&msg));
            })
            .expect("не встал поток записи файла");
    }

    fn file_get(&self, hub: &Hub<ClientState>, client: u64, id: Option<u64>, phone: PhoneId, src: String) {
        let fail = |detail: String| hub.error(client, id, detail);
        let (files, serial) = match self.file_target(phone, &src) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        let (size, reader) = match files.backend.get(&serial, &src) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        let transfer = self.shared.next_transfer.fetch_add(1, Ordering::SeqCst);
        let cancel = Arc::new(AtomicBool::new(false));
        hub.with_clients(|m| {
            if let Some(c) = m.get_mut(&client) {
                c.data.transfers.insert(transfer, Transfer { tx: None, cancel: cancel.clone() });
            }
        });
        hub.reply(client, id, to_value(&ServerMsg::FileInfo { transfer, size }));
        let hub = hub.clone();
        thread::Builder::new()
            .name(format!("file-get-{transfer}"))
            .spawn(move || {
                let result = pump_get(&hub, client, transfer, size, reader, &cancel);
                hub.with_clients(|m| {
                    if let Some(c) = m.get_mut(&client) {
                        c.data.transfers.remove(&transfer);
                    }
                });
                let msg = match result {
                    Ok(()) => ServerMsg::FileDone { transfer, size },
                    Err(detail) => ServerMsg::FileFailed { transfer, detail },
                };
                hub.reply(client, None, to_value(&msg));
            })
            .expect("не встал поток чтения файла");
    }
}

/// Принимает данные от клиента и пишет их на телефон до заявленного размера.
#[allow(clippy::too_many_arguments)]
fn pump_put(
    hub: &Hub<ClientState>,
    client: u64,
    transfer: u32,
    size: u64,
    media: bool,
    mut writer: Box<dyn super::files::FileWriter>,
    rx: Receiver<Vec<u8>>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut received = 0u64;
    let mut next_report = 1024 * 1024;
    while received < size {
        if cancel.load(Ordering::SeqCst) {
            return Err("передача отменена".into());
        }
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(chunk) => {
                received += chunk.len() as u64;
                if received > size {
                    return Err("получено больше заявленного размера".into());
                }
                writer.write(&chunk)?;
                if received >= next_report {
                    next_report = received + 1024 * 1024;
                    hub.reply(client, None, to_value(&ServerMsg::FileProgress { transfer, bytes: received }));
                }
            }
            Err(RecvTimeoutError::Timeout) => return Err("клиент перестал присылать данные".into()),
            Err(RecvTimeoutError::Disconnected) => return Err("соединение с клиентом закрыто".into()),
        }
    }
    writer.finish(size, media)
}

/// Читает файл с телефона и отдаёт клиенту пакетами; медленный клиент притормаживает чтение.
fn pump_get(
    hub: &Hub<ClientState>,
    client: u64,
    transfer: u32,
    size: u64,
    mut reader: Box<dyn Read + Send>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let out = hub.with_clients(|m| m.get(&client).map(|c| c.out.clone())).ok_or("клиент ушёл")?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut sent = 0u64;
    let mut next_report = 1024 * 1024;
    while sent < size {
        if cancel.load(Ordering::SeqCst) {
            return Err("передача отменена".into());
        }
        let n = reader.read(&mut buf).map_err(|e| format!("чтение файла: {e}"))?;
        if n == 0 {
            return Err("файл прочитан не полностью".into());
        }
        let mut payload = Vec::with_capacity(4 + n);
        payload.extend_from_slice(&transfer.to_le_bytes());
        payload.extend_from_slice(&buf[..n]);
        if !out.bulk(PKG_FILE, payload) {
            return Err("клиент отключился".into());
        }
        sent += n as u64;
        if sent >= next_report {
            next_report = sent + 1024 * 1024;
            hub.reply(client, None, to_value(&ServerMsg::FileProgress { transfer, bytes: sent }));
        }
    }
    // `FileDone` (JSON) обгоняет данные, если они ещё ждут в очереди: дожидаемся, пока клиент всё получит.
    if !out.wait_bulk_drained(Duration::from_secs(60)) {
        return Err("клиент не забрал данные".into());
    }
    Ok(())
}

/// Маршрутизация события фермы по подпискам.
fn route(hub: &Hub<ClientState>, ev: FarmEvent) {
    match ev {
        FarmEvent::Video { phone, frame } => {
            let payload =
                encode_video(phone, frame.seq, frame.codec, frame.key_frame, frame.pts_us, &frame.data);
            let now = Instant::now();
            let mut hash = None;
            hub.with_clients(|map| {
                for c in map.values_mut().filter(|c| c.authed) {
                    let Some(w) = c.data.watches.get_mut(&phone) else { continue };
                    if let (Some(fps), Some(t)) = (w.max_fps, w.last_sent) {
                        // Небольшой допуск, чтобы 30 к/с из 30 к/с не терялись на дрожании времени.
                        if now.duration_since(t).as_secs_f32() < 0.9 / fps {
                            continue;
                        }
                    }
                    if w.on_change {
                        let h = *hash.get_or_insert_with(|| frame_hash(&frame.data));
                        if w.last_hash == Some(h) {
                            continue;
                        }
                        w.last_hash = Some(h);
                    }
                    w.last_sent = Some(now);
                    c.out.latest(PKG_VIDEO, payload.clone());
                }
            });
        }
        FarmEvent::Audio { phone, pts_us, data } => {
            let payload = encode_audio(phone, pts_us, &data);
            hub.with_clients(|map| {
                for c in map.values().filter(|c| c.authed) {
                    if c.data.watches.get(&phone).is_some_and(|w| w.audio) {
                        c.out.lossy(PKG_AUDIO, payload.clone());
                    }
                }
            });
        }
        // Обнаружение и состояния касаются всех клиентов: подписки на телефон ещё может не быть.
        ev @ (FarmEvent::Appeared { .. } | FarmEvent::State { .. } | FarmEvent::Lost { .. }) => {
            let (_, msg) = map_daemon_event(ev);
            hub.broadcast(|_| true, &to_value(&msg));
        }
        // Ответы приложения и его сообщения (передний план, «экран изменился») — ещё и тем, кто спрашивал приложение.
        ev @ FarmEvent::Status { .. } => {
            let (phone, msg) = map_daemon_event(ev);
            hub.broadcast(
                |c| c.data.watches.contains_key(&phone) || c.data.apps.contains(&phone),
                &to_value(&msg),
            );
        }
        other => {
            let (phone, msg) = map_daemon_event(other);
            hub.broadcast(|c| c.data.watches.contains_key(&phone), &to_value(&msg));
        }
    }
}

/// Сведения о телефоне для списка.
fn phone_json(i: &pcdock_farm::PhoneInfo) -> Value {
    json!({
        "phone": i.id,
        "state": state_name(i.state),
        "transport": i.transport.map(|t| t.to_string()),
        "address": i.address,
        "serial": i.serial,
        "name": i.name,
        "display": i.display.as_ref().map(|d| json!({
            "stream_w": d.stream_w, "stream_h": d.stream_h, "screen_w": d.screen_w,
            "screen_h": d.screen_h, "dpi": d.dpi, "rotation": d.rotation,
        })),
    })
}

fn state_name(s: PhoneState) -> &'static str {
    match s {
        PhoneState::Appeared => "appeared",
        PhoneState::Connecting => "connecting",
        PhoneState::Ready => "ready",
        PhoneState::NeedsPermission => "needs_permission",
        PhoneState::NeedsApp => "needs_app",
        PhoneState::NeedsAccess => "needs_access",
        PhoneState::NeedsAuth => "needs_auth",
        PhoneState::Lost => "lost",
    }
}

fn map_daemon_event(ev: FarmEvent) -> (PhoneId, ServerMsg) {
    match ev {
        FarmEvent::Connected { phone, version, capabilities, name } => {
            (phone, ServerMsg::Connected { phone, version, caps: capabilities, name })
        }
        FarmEvent::Display { phone, stream_w, stream_h, screen_w, screen_h, dpi, rotation } => {
            (phone, ServerMsg::Display { phone, stream_w, stream_h, screen_w, screen_h, dpi, rotation })
        }
        FarmEvent::AudioConfig { phone, codec, rate, channels } => {
            (phone, ServerMsg::AudioConfig { phone, codec, rate, channels })
        }
        FarmEvent::Status { phone, code, detail } => (phone, ServerMsg::Status { phone, code, detail }),
        FarmEvent::PeerSilent { phone } => (phone, ServerMsg::PeerSilent { phone }),
        FarmEvent::Malformed { phone, detail } => (phone, ServerMsg::Malformed { phone, detail }),
        FarmEvent::Disconnected { phone, reason } => (phone, ServerMsg::Disconnected { phone, reason }),
        FarmEvent::LinkDown { phone, detail } => (phone, ServerMsg::LinkDown { phone, detail }),
        FarmEvent::Warning { phone, detail } => (phone, ServerMsg::Warning { phone, detail }),
        FarmEvent::Appeared { phone, transport, address, serial } => {
            (phone, ServerMsg::Appeared { phone, transport: transport.to_string(), address, serial })
        }
        FarmEvent::State { phone, state, detail } => {
            (phone, ServerMsg::State { phone, state: state_name(state).into(), detail })
        }
        FarmEvent::Lost { phone } => (phone, ServerMsg::Lost { phone }),
        FarmEvent::Video { .. } | FarmEvent::Audio { .. } => {
            unreachable!("видео и звук идут бинарными пакетами")
        }
    }
}

/// Команда протокола в команду демона. Коды: kind 0 тач, 1 мышь, 2 клавиатура;
/// phase 0/1/2/3 down/move/up/cancel; button 0/1/2 левая/правая/средняя.
fn map_input(msg: ClientMsg) -> Result<(PhoneId, InputCmd), String> {
    let bad = |what: &str| format!("плохая команда: {what}");
    let parse_kind = |k: u32| match k {
        0 => Ok(DeviceKind::Touch),
        1 => Ok(DeviceKind::Mouse),
        2 => Ok(DeviceKind::Keyboard),
        _ => Err(bad("kind")),
    };
    let parse_phase = |p: u32| match p {
        0 => Ok(TouchPhase::Down),
        1 => Ok(TouchPhase::Move),
        2 => Ok(TouchPhase::Up),
        3 => Ok(TouchPhase::Cancel),
        _ => Err(bad("phase")),
    };
    let parse_button = |b: u32| match b {
        0 => Ok(MouseButton::Left),
        1 => Ok(MouseButton::Right),
        2 => Ok(MouseButton::Middle),
        _ => Err(bad("button")),
    };
    Ok(match msg {
        ClientMsg::TouchEx { phone, cid, phase, x, y, width, height, pressure } => {
            (phone, InputCmd::touch_ex(cid, parse_phase(phase)?, x, y, width, height, pressure))
        }
        ClientMsg::CursorMoveAbs { phone, x, y } => (phone, InputCmd::CursorMoveAbs { x, y }),
        ClientMsg::CursorMoveRel { phone, dx, dy } => (phone, InputCmd::CursorMoveRel { dx, dy }),
        ClientMsg::CursorResync { phone } => (phone, InputCmd::CursorResync),
        ClientMsg::MouseButton { phone, button, down } => {
            (phone, InputCmd::MouseButton { button: parse_button(button)?, down })
        }
        ClientMsg::Scroll { phone, ticks } => (phone, InputCmd::Scroll { ticks }),
        ClientMsg::ScrollH { phone, ticks } => (phone, InputCmd::ScrollH { ticks }),
        ClientMsg::Key { phone, usage, down } => (phone, InputCmd::Key { usage, down }),
        ClientMsg::DeviceEnable { phone, kind, on } => {
            (phone, InputCmd::DeviceEnable { kind: parse_kind(kind)?, on })
        }
        ClientMsg::ReleaseAll { phone } => (phone, InputCmd::ReleaseAll),
        ClientMsg::RequestFrame { phone } => (phone, InputCmd::RequestFrame),
        ClientMsg::Wake { phone } => (phone, InputCmd::Wake),
        ClientMsg::SetStream { phone, video, audio } => {
            (phone, InputCmd::SetStream { video, audio, max_fps: 0 })
        }
        ClientMsg::LaunchApp { phone, request, package } => {
            (phone, InputCmd::App(AppRequest::LaunchApp { request, package }))
        }
        ClientMsg::ListApps { phone, request } => (phone, InputCmd::App(AppRequest::ListApps { request })),
        ClientMsg::UiDump { phone, request } => (phone, InputCmd::App(AppRequest::UiDump { request })),
        ClientMsg::ExitInfo { phone, request, since_ms, package } => {
            (phone, InputCmd::App(AppRequest::ExitInfo { request, since_ms, package }))
        }
        ClientMsg::IpCheck { phone, request, url } => {
            (phone, InputCmd::App(AppRequest::IpCheck { request, url }))
        }
        ClientMsg::HttpLocal { phone, request, method, url, headers, body, timeout_ms } => (
            phone,
            InputCmd::App(AppRequest::HttpLocal {
                request,
                method,
                url,
                headers: headers.into_iter().collect(),
                body,
                timeout_ms,
            }),
        ),
        ClientMsg::TypeText { phone, request, text } => {
            (phone, InputCmd::App(AppRequest::TypeText { request, text }))
        }
        ClientMsg::Auth { .. }
        | ClientMsg::List
        | ClientMsg::Watch { .. }
        | ClientMsg::Unwatch { .. }
        | ClientMsg::Snapshot { .. }
        | ClientMsg::FilePut { .. }
        | ClientMsg::FileGet { .. }
        | ClientMsg::FileList { .. }
        | ClientMsg::FileDelete { .. }
        | ClientMsg::FileCancel { .. }
        | ClientMsg::SetTransport { .. }
        | ClientMsg::AppEvents { .. }
        | ClientMsg::Pin { .. }
        | ClientMsg::Unpin { .. } => return Err(bad("не команда ввода")),
    })
}
