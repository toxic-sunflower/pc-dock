//! Сессия на стороне хоста (PROTOCOL.md §5): рукопожатие, согласование версии, keep-alive,
//! разбор входящего потока и команды ввода.
//!
//! Модуль не выполняет ввод-вывод и не читает часы. Адаптер:
//! 1. передаёт прочитанные из транспорта байты в [`Session::handle_input`];
//! 2. раз в [`Session::next_timeout`] вызывает [`Session::handle_timeout`];
//! 3. забирает через [`Session::poll_transmit`] то, что нужно отправить (байты потока или USB-запросы);
//! 4. забирает через [`Session::poll_event`] события для фронтенда.
//!
//! Время (`now_ms`) — любые монотонные миллисекунды, их даёт вызывающий.

use crate::hid::{AoaStrings, ControlRequest};
use crate::input::{DeviceKind, HidConfig, Input, InputError, MouseButton, TouchPoint};
use crate::packet::*;
use std::collections::VecDeque;

/// Настройки SDK. Всё, что здесь есть, пользователь библиотеки может менять.
#[derive(Clone, Debug)]
pub struct Config {
    /// Имя реализации для `Hello` (например, "mygame-tester/1.0").
    pub name: String,
    /// Возможности, которые хост готов принимать (`packet::caps`).
    pub capabilities: u32,
    /// Строки AOA. Должны совпадать с фильтром приложения-приёмника.
    pub aoa: AoaStrings,
    pub hid: HidConfig,
    /// Если задано, отправляется телефону сразу после согласования.
    pub stream: Option<SetStream>,
    /// Как часто слать `PING`.
    pub ping_interval_ms: u64,
    /// Через сколько без пакетов от телефона считать его недоступным.
    pub peer_timeout_ms: u64,
    /// Как часто повторять `HELLO`, пока телефон не ответил.
    pub hello_retry_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: format!("pcdock-core/{}", env!("CARGO_PKG_VERSION")),
            capabilities: caps::VIDEO_JPEG
                | caps::AUDIO_PCM
                | caps::AUDIO_OPUS
                | caps::ROTATION
                | caps::FOREGROUND
                | caps::LAUNCH_APP
                | caps::LIST_APPS
                | caps::UI_TREE
                | caps::EXIT_INFO
                | caps::IP_CHECK
                | caps::HTTP_LOCAL
                | caps::TEXT_INPUT,
            aoa: AoaStrings::default(),
            hid: HidConfig::default(),
            stream: None,
            ping_interval_ms: 2000,
            peer_timeout_ms: 6000,
            hello_retry_ms: 1000,
        }
    }
}

/// То, что адаптер должен отправить.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transmit {
    /// Записать байты в транспорт (bulk OUT / сокет).
    Stream(Vec<u8>),
    /// Выполнить USB control transfer.
    Control(ControlRequest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
    /// От телефона давно нет пакетов.
    Timeout,
    /// Версии протокола несовместимы.
    VersionMismatch,
    /// Телефон прислал `ERROR`.
    PeerError(ErrorMsg),
    /// Закрыто вызовом [`Session::close`] (например, пропал USB).
    Closed,
}

/// События для фронтенда.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Connected {
        version: u16,
        capabilities: u32,
        peer_name: String,
    },
    Display(DisplayInfo),
    Video(VideoFrame),
    AudioConfig(AudioConfig),
    Audio(AudioFrame),
    Status(Status),
    /// Телефон не ответил на `HELLO` за `peer_timeout_ms`. Вероятно, приложение-приёмник не запущено:
    /// адаптер может сбросить USB, чтобы телефон заново прошёл через режим аксессуара.
    PeerSilent,
    /// Пришёл пакет, который не удалось разобрать (он пропущен).
    Malformed(String),
    Disconnected(DisconnectReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Ещё не запущена или остановлена после закрытия.
    Idle,
    /// `HELLO` отправлен, ждём ответ телефона.
    Connecting,
    Active,
}

/// Почему `Session::launch_app` не отправил команду.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchError {
    /// Сессия не активна.
    NotActive,
    /// Телефон не умеет (`caps::LAUNCH_APP` не согласован).
    Unsupported,
    /// Некорректное имя пакета.
    BadPackage,
    /// Некорректный URL для `IP_CHECK` (только `https://`, см. `packet::valid_ip_check_url`).
    BadUrl,
    /// Некорректный запрос `HTTP_LOCAL` (метод, заголовки или тело, см. `packet::HttpLocalReq::check`) или текст
    /// `TEXT_INPUT` (пустой, длиннее 16 КБ или с `\0`, см. `packet::valid_text_input`).
    BadRequest,
}

#[derive(Debug)]
pub struct Session {
    cfg: Config,
    state: State,
    reader: PacketReader,
    tx: VecDeque<Transmit>,
    events: VecDeque<Event>,
    input: Input,
    version: u16,
    negotiated_caps: u32,
    display: Option<DisplayInfo>,
    started_ms: u64,
    hello_sent_ms: u64,
    last_rx_ms: u64,
    last_ping_ms: u64,
    nonce: u64,
    silent_reported: bool,
}

impl Session {
    pub fn new(cfg: Config) -> Self {
        let input = Input::new(cfg.hid.clone());
        Self {
            cfg,
            state: State::Idle,
            reader: PacketReader::new(),
            tx: VecDeque::new(),
            events: VecDeque::new(),
            input,
            version: 0,
            negotiated_caps: 0,
            display: None,
            started_ms: 0,
            hello_sent_ms: 0,
            last_rx_ms: 0,
            last_ping_ms: 0,
            nonce: 0,
            silent_reported: false,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn input(&self) -> &Input {
        &self.input
    }

    /// Последняя полученная информация об экране.
    pub fn display(&self) -> Option<DisplayInfo> {
        self.display
    }

    pub fn negotiated_version(&self) -> u16 {
        self.version
    }

    pub fn negotiated_capabilities(&self) -> u32 {
        self.negotiated_caps
    }

    /// Сообщает, сколько байт отброшено при перестройке потока.
    pub fn dropped_bytes(&self) -> u64 {
        self.reader.dropped_bytes()
    }

    fn local_hello(&self) -> Hello {
        Hello {
            version_max: PROTOCOL_VERSION,
            version_min: PROTOCOL_VERSION,
            capabilities: self.cfg.capabilities,
            role: Role::Host,
            name: self.cfg.name.clone(),
        }
    }

    fn send_packet(&mut self, p: Packet) {
        self.tx.push_back(Transmit::Stream(p.encode()));
    }

    fn queue_controls(&mut self, requests: Vec<ControlRequest>) {
        self.tx.extend(requests.into_iter().map(Transmit::Control));
    }

    /// Телефон перешёл в режим аксессуара и транспорт открыт: начинаем сессию.
    /// Отправляет `HELLO` и регистрирует включённые устройства ввода.
    pub fn start(&mut self, now_ms: u64) {
        self.reader.clear();
        self.display = None;
        self.version = 0;
        self.negotiated_caps = 0;
        self.state = State::Connecting;
        self.started_ms = now_ms;
        self.hello_sent_ms = now_ms;
        self.last_rx_ms = now_ms;
        self.last_ping_ms = now_ms;
        self.silent_reported = false;
        let hello = self.local_hello().to_packet();
        self.send_packet(hello);
        let mut ctrl = Vec::new();
        self.input.start(&mut ctrl);
        self.queue_controls(ctrl);
    }

    /// Транспорт потерян (например, кабель выдернули). Сессия закрывается, ввод сбрасывается.
    pub fn close(&mut self) {
        if self.state != State::Idle {
            self.finish(DisconnectReason::Closed);
        }
    }

    fn finish(&mut self, reason: DisconnectReason) {
        self.state = State::Idle;
        self.input.stop();
        self.events.push_back(Event::Disconnected(reason));
    }

    /// Байты, прочитанные из транспорта.
    pub fn handle_input(&mut self, now_ms: u64, data: &[u8]) {
        if self.state == State::Idle {
            return;
        }
        self.reader.push(data);
        while let Some(packet) = self.reader.next_packet() {
            if self.state == State::Idle {
                break;
            }
            self.last_rx_ms = now_ms;
            self.handle_packet(packet);
        }
    }

    fn malformed(&mut self, what: &str, e: DecodeError) {
        self.events.push_back(Event::Malformed(format!("{what}: {e}")));
    }

    fn handle_packet(&mut self, packet: Packet) {
        match packet.ptype {
            PacketType::Hello => self.handle_hello(&packet),
            PacketType::Error => match ErrorMsg::from_payload(&packet.payload) {
                Ok(e) => {
                    self.events.push_back(Event::Status(Status { code: e.code, detail: e.message.clone() }));
                    self.finish(DisconnectReason::PeerError(e));
                }
                Err(e) => self.malformed("ERROR", e),
            },
            _ if self.state != State::Active => {} // до согласования принимаем только HELLO/ERROR
            PacketType::Ping => match nonce_from_payload(&packet.payload) {
                Ok(n) => self.send_packet(pong_packet(n)),
                Err(e) => self.malformed("PING", e),
            },
            PacketType::Pong => {}
            PacketType::DisplayInfo => match DisplayInfo::from_payload(&packet.payload) {
                Ok(d) => self.handle_display(d),
                Err(e) => self.malformed("DISPLAY_INFO", e),
            },
            PacketType::VideoFrame => match VideoFrame::from_packet(&packet) {
                Ok(f) => self.events.push_back(Event::Video(f)),
                Err(e) => self.malformed("VIDEO_FRAME", e),
            },
            PacketType::AudioConfig => match AudioConfig::from_payload(&packet.payload) {
                Ok(c) => self.events.push_back(Event::AudioConfig(c)),
                Err(e) => self.malformed("AUDIO_CONFIG", e),
            },
            PacketType::AudioFrame => match AudioFrame::from_payload(&packet.payload) {
                Ok(f) => self.events.push_back(Event::Audio(f)),
                Err(e) => self.malformed("AUDIO_FRAME", e),
            },
            PacketType::Status => match Status::from_payload(&packet.payload) {
                Ok(s) => self.events.push_back(Event::Status(s)),
                Err(e) => self.malformed("STATUS", e),
            },
            // VideoConfig, RequestFrame, SetStream и неизвестные типы хосту ни к чему: пропускаем.
            _ => {}
        }
    }

    fn handle_hello(&mut self, packet: &Packet) {
        let remote = match Hello::from_payload(&packet.payload) {
            Ok(h) => h,
            Err(e) => return self.malformed("HELLO", e),
        };
        if remote.role != Role::Device {
            return; // эхо собственного HELLO или чужая роль
        }
        match negotiate(&self.local_hello(), &remote) {
            Err(code) => {
                let message =
                    format!("версия {} не поддерживается (нужна {})", remote.version_max, PROTOCOL_VERSION);
                self.send_packet(ErrorMsg { code, message }.to_packet());
                self.finish(DisconnectReason::VersionMismatch);
            }
            Ok((version, capabilities)) => {
                self.version = version;
                self.negotiated_caps = capabilities;
                self.state = State::Active;
                self.last_ping_ms = self.last_rx_ms;
                self.events.push_back(Event::Connected { version, capabilities, peer_name: remote.name });
                if let Some(stream) = self.cfg.stream {
                    self.send_packet(stream.to_packet());
                }
            }
        }
    }

    fn handle_display(&mut self, d: DisplayInfo) {
        self.display = Some(d);
        self.input.set_screen(u32::from(d.screen_width), u32::from(d.screen_height));
        let mut ctrl = Vec::new();
        // rotation уже проверен при разборе (0..3), ошибки быть не может
        let _ = self.input.set_rotation(d.rotation, &mut ctrl);
        self.queue_controls(ctrl);
        self.events.push_back(Event::Display(d));
    }

    /// Вызывается, когда наступило время из [`Session::next_timeout`].
    pub fn handle_timeout(&mut self, now_ms: u64) {
        match self.state {
            State::Idle => {}
            State::Connecting => {
                if now_ms.saturating_sub(self.hello_sent_ms) >= self.cfg.hello_retry_ms {
                    self.hello_sent_ms = now_ms;
                    let hello = self.local_hello().to_packet();
                    self.send_packet(hello);
                }
                if !self.silent_reported && now_ms.saturating_sub(self.started_ms) >= self.cfg.peer_timeout_ms
                {
                    self.silent_reported = true;
                    self.events.push_back(Event::PeerSilent);
                }
            }
            State::Active => {
                if now_ms.saturating_sub(self.last_rx_ms) >= self.cfg.peer_timeout_ms {
                    return self.finish(DisconnectReason::Timeout);
                }
                if now_ms.saturating_sub(self.last_ping_ms) >= self.cfg.ping_interval_ms {
                    self.last_ping_ms = now_ms;
                    self.nonce += 1;
                    let ping = ping_packet(self.nonce);
                    self.send_packet(ping);
                }
            }
        }
    }

    /// Когда нужно вызвать [`Session::handle_timeout`] в следующий раз (в тех же миллисекундах).
    pub fn next_timeout(&self) -> Option<u64> {
        match self.state {
            State::Idle => None,
            State::Connecting => {
                let retry = self.hello_sent_ms + self.cfg.hello_retry_ms;
                let silent = (!self.silent_reported).then(|| self.started_ms + self.cfg.peer_timeout_ms);
                Some(silent.map_or(retry, |s| s.min(retry)))
            }
            State::Active => {
                let ping = self.last_ping_ms + self.cfg.ping_interval_ms;
                let dead = self.last_rx_ms + self.cfg.peer_timeout_ms;
                Some(ping.min(dead))
            }
        }
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.tx.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    // ----- команды управления потоком -----

    /// Запомнить настройки потока: отправятся телефону сразу после согласования.
    /// (Удобно для FFI/фронтендов: вызвать до `start`. Отправка вживую — через `set_stream`.)
    pub fn set_initial_stream(&mut self, settings: SetStream) {
        self.cfg.stream = Some(settings);
    }

    /// Просит телефон изменить параметры потока. Телефон ответит новым `DisplayInfo`.
    /// Настройки запоминаются: после переподключения телефон получит последние.
    pub fn set_stream(&mut self, settings: SetStream) {
        self.cfg.stream = Some(settings);
        if self.state == State::Active {
            self.send_packet(settings.to_packet());
        }
    }

    /// Включить или выключить видео и звук и сменить предел частоты, не трогая размер и качество кадра,
    /// заданные раньше (`set_initial_stream`/`set_stream`). Так `InputCmd::SetStream` не сбрасывает их.
    pub fn set_stream_switches(&mut self, video: bool, audio: bool, max_fps: u8) {
        let base = self.cfg.stream.unwrap_or_default();
        self.set_stream(SetStream { video, audio, max_fps, ..base });
    }

    /// Просит телефон прислать текущее содержимое экрана.
    pub fn request_frame(&mut self) {
        if self.state == State::Active {
            self.send_packet(request_frame_packet());
        }
    }

    /// Просит телефон включить экран на минуту (разблокировка вручную).
    pub fn wake(&mut self) {
        if self.state == State::Active {
            self.send_packet(wake_packet());
        }
    }

    /// Просит телефон открыть приложение (`LAUNCH_APP`, PROTOCOL.md §5 «Запуск приложения»). Ответ придёт
    /// событием `Status` с кодом 18 и `detail` = `"<request_id> <результат>"`.
    /// Ошибка — сессия не активна, телефон не согласовал `caps::LAUNCH_APP` или имя пакета некорректно.
    pub fn launch_app(&mut self, request_id: u32, package: &str) -> Result<(), LaunchError> {
        if self.state != State::Active {
            return Err(LaunchError::NotActive);
        }
        if self.negotiated_caps & caps::LAUNCH_APP == 0 {
            return Err(LaunchError::Unsupported);
        }
        if !valid_package(package) {
            return Err(LaunchError::BadPackage);
        }
        let p = LaunchApp { request_id, package: package.to_string() }.to_packet();
        self.send_packet(p);
        Ok(())
    }

    /// Просит телефон прислать список приложений с главным экраном (`LIST_APPS`). Ответ придёт событием `Status`
    /// с кодом 19 и `detail` = `"<request_id> <JSON>"`. Ошибка — сессия не активна или `caps::LIST_APPS` не согласован.
    pub fn list_apps(&mut self, request_id: u32) -> Result<(), LaunchError> {
        if self.state != State::Active {
            return Err(LaunchError::NotActive);
        }
        if self.negotiated_caps & caps::LIST_APPS == 0 {
            return Err(LaunchError::Unsupported);
        }
        self.send_packet(list_apps_packet(request_id));
        Ok(())
    }

    /// Выполнить запрос к приложению на телефоне. Ответ всегда придёт событием `Status` (см. [`crate::link::AppRequest`]):
    /// если запрос не ушёл, сессия сама кладёт `Status` с `"<request> error <причина>"`.
    pub fn app_request(&mut self, req: &crate::link::AppRequest) {
        use crate::link::AppRequest as R;
        let r = match req {
            R::LaunchApp { request, package } => self.launch_app(*request, package),
            R::ListApps { request } => self.list_apps(*request),
            R::UiDump { request } => self.ui_dump(*request),
            R::ExitInfo { request, since_ms, package } => self.exit_info(*request, *since_ms, package),
            R::IpCheck { request, url } => self.ip_check(*request, url),
            R::HttpLocal { request, method, url, headers, body, timeout_ms } => {
                self.http_local(&HttpLocalReq {
                    request_id: *request,
                    method: method.clone(),
                    url: url.clone(),
                    headers: headers.clone(),
                    body: body.clone(),
                    timeout_ms: *timeout_ms,
                })
            }
            R::TypeText { request, text } => {
                self.text_input(&TextInputReq { request_id: *request, text: text.clone() })
            }
        };
        if let Err(e) = r {
            let why = match e {
                LaunchError::NotActive => "not_active",
                LaunchError::Unsupported => "unsupported",
                LaunchError::BadPackage => "invalid_package",
                LaunchError::BadUrl => "invalid_url",
                LaunchError::BadRequest => "invalid_request",
            };
            let part = if matches!(req, R::UiDump { .. }) { "1/1 " } else { "" };
            let detail = format!("{} {part}error {why}", req.request());
            self.events.push_back(Event::Status(Status { code: req.reply_code(), detail }));
        }
    }

    /// Просит телефон прислать элементы экрана (`UI_DUMP`). Ответ придёт событиями `Status` с кодом 20
    /// (`"<request_id> <i>/<n> <часть>"`). Ошибка — сессия не активна или `caps::UI_TREE` не согласован.
    pub fn ui_dump(&mut self, request_id: u32) -> Result<(), LaunchError> {
        if self.state != State::Active {
            return Err(LaunchError::NotActive);
        }
        if self.negotiated_caps & caps::UI_TREE == 0 {
            return Err(LaunchError::Unsupported);
        }
        self.send_packet(ui_dump_packet(request_id));
        Ok(())
    }

    /// Просит телефон прислать причины завершения процессов приложения после `since_ms` (`EXIT_INFO`). Ответ —
    /// событие `Status` с кодом 22. Ошибка — сессия не активна, `caps::EXIT_INFO` не согласован или имя пакета неверно.
    pub fn exit_info(&mut self, request_id: u32, since_ms: u64, package: &str) -> Result<(), LaunchError> {
        if self.state != State::Active {
            return Err(LaunchError::NotActive);
        }
        if self.negotiated_caps & caps::EXIT_INFO == 0 {
            return Err(LaunchError::Unsupported);
        }
        if !valid_package(package) {
            return Err(LaunchError::BadPackage);
        }
        self.send_packet(ExitInfoReq { request_id, since_ms, package: package.to_string() }.to_packet());
        Ok(())
    }

    /// Просит телефон сделать HTTPS GET по своей сети по умолчанию (`IP_CHECK`) — внешний IP телефона. Ответ придёт
    /// событием `Status` с кодом 23 и `detail` = `"<request_id> <JSON>"`. Ошибка — сессия не активна, `caps::IP_CHECK`
    /// не согласован или URL некорректен.
    pub fn ip_check(&mut self, request_id: u32, url: &str) -> Result<(), LaunchError> {
        if self.state != State::Active {
            return Err(LaunchError::NotActive);
        }
        if self.negotiated_caps & caps::IP_CHECK == 0 {
            return Err(LaunchError::Unsupported);
        }
        if !valid_ip_check_url(url) {
            return Err(LaunchError::BadUrl);
        }
        self.send_packet(IpCheckReq { request_id, url: url.to_string() }.to_packet());
        Ok(())
    }

    /// Просит телефон ввести текст в поле с курсором (`TEXT_INPUT`). Ответ придёт событием `Status` с кодом 25 и
    /// `detail` = `"<request_id> <JSON>"`; при `"method":"clipboard"` текст только в буфере обмена — нажать Ctrl+V
    /// должен хост. Ошибка — сессия не активна, `caps::TEXT_INPUT` не согласован или текст некорректен (`BadRequest`).
    pub fn text_input(&mut self, req: &TextInputReq) -> Result<(), LaunchError> {
        if self.state != State::Active {
            return Err(LaunchError::NotActive);
        }
        if self.negotiated_caps & caps::TEXT_INPUT == 0 {
            return Err(LaunchError::Unsupported);
        }
        if !req.valid() {
            return Err(LaunchError::BadRequest);
        }
        self.send_packet(req.to_packet());
        Ok(())
    }

    /// Просит телефон выполнить HTTP-запрос к устройству своей локальной Wi-Fi-сети (`HTTP_LOCAL`), например к админке
    /// роутера. Ответ придёт событием `Status` с кодом 24 и `detail` = `"<request_id> <JSON>"`. Ошибка — сессия не
    /// активна, `caps::HTTP_LOCAL` не согласован, URL (`BadUrl`) или метод/заголовки/тело (`BadRequest`) некорректны.
    pub fn http_local(&mut self, req: &HttpLocalReq) -> Result<(), LaunchError> {
        if self.state != State::Active {
            return Err(LaunchError::NotActive);
        }
        if self.negotiated_caps & caps::HTTP_LOCAL == 0 {
            return Err(LaunchError::Unsupported);
        }
        match req.check() {
            Ok(()) => {}
            Err(HttpLocalInvalid::Url) => return Err(LaunchError::BadUrl),
            Err(HttpLocalInvalid::Request) => return Err(LaunchError::BadRequest),
        }
        self.send_packet(req.to_packet());
        Ok(())
    }

    // ----- команды ввода (PROTOCOL.md §8) -----

    fn with_input<R>(&mut self, f: impl FnOnce(&mut Input, &mut Vec<ControlRequest>) -> R) -> R {
        let mut ctrl = Vec::new();
        let result = f(&mut self.input, &mut ctrl);
        self.queue_controls(ctrl);
        result
    }

    /// Включает или отключает устройство ввода (отключение снимает его с телефона).
    pub fn device_enable(&mut self, kind: DeviceKind, enabled: bool) {
        self.with_input(|i, out| i.set_enabled(kind, enabled, out));
    }

    pub fn touch(&mut self, point: TouchPoint) -> Result<(), InputError> {
        self.with_input(|i, out| i.touch(point, out))
    }

    pub fn cursor_move_abs(&mut self, x: f32, y: f32) -> Result<(), InputError> {
        self.with_input(|i, out| i.cursor_move_abs(x, y, out))
    }

    pub fn cursor_move_rel(&mut self, dx: i32, dy: i32) -> Result<(), InputError> {
        self.with_input(|i, out| i.cursor_move_rel(dx, dy, out))
    }

    pub fn cursor_resync(&mut self) {
        self.input.cursor_resync();
    }

    pub fn mouse_button(&mut self, button: MouseButton, down: bool) -> Result<(), InputError> {
        self.with_input(|i, out| i.mouse_button(button, down, out))
    }

    pub fn scroll(&mut self, ticks: f32) -> Result<(), InputError> {
        self.with_input(|i, out| i.scroll(ticks, out))
    }

    /// Горизонтальная прокрутка (Consumer AC Pan → REL_HWHEEL на телефоне).
    pub fn scroll_h(&mut self, ticks: f32) -> Result<(), InputError> {
        self.with_input(|i, out| i.scroll_h(ticks, out))
    }

    pub fn key(&mut self, usage: u16, down: bool) -> Result<(), InputError> {
        self.with_input(|i, out| i.key(usage, down, out))
    }

    /// Отпускает всё нажатое (например, при потере фокуса окна).
    pub fn release_all(&mut self) {
        self.with_input(|i, out| i.release_all(out));
    }
}
