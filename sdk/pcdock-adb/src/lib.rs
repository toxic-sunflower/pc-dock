//! ADB-транспорт: связь с телефоном через ADB-сервер.
//!
//! Как устроено: демон говорит с локальным ADB-сервером по TCP
//! (127.0.0.1:5037), просит список устройств и пробрасывает порт
//! (`forward`) на сокет приложения в телефоне. Дальше по этому TCP
//! идут те же пакеты v2, что и по USB bulk: сессия ядра разницы не видит.
//!
//! Телефонная сторона (сокет в приложении) — следующим шагом; без неё
//! `connect` честно сообщает, что трубке не во что воткнуться.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use pcdock_core::input::TouchPoint;
use pcdock_core::link::{InputCmd, LinkError, PhoneLink};
use pcdock_core::session::{Config, Event, Session, Transmit};

/// Таймауты по умолчанию.
pub const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// Источник фактов для фермы: устройства, видимые ADB-серверу. Нет сервера — пусто.
pub struct AdbDiscovery {
    adb: Adb,
}

impl AdbDiscovery {
    pub fn new(adb: Adb) -> Self {
        Self { adb }
    }
}

impl pcdock_core::link::Discovery for AdbDiscovery {
    fn scan(&mut self) -> Vec<pcdock_core::link::Sighting> {
        use pcdock_core::link::{Issue, Sighting, Transport};
        self.adb
            .device_states()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(serial, state)| {
                let issue = match state.as_str() {
                    "device" => None,
                    "unauthorized" => Some(Issue::Unauthorized),
                    // offline, bootloader и прочее: не наш случай или временно.
                    _ => return None,
                };
                Some((serial, issue))
            })
            .map(|(serial, issue)| Sighting {
                transport: Transport::Adb,
                address: serial.clone(),
                serial: Some(serial),
                issue,
            })
            .collect()
    }
}

/// Минимальный клиент ADB-сервера (протокол smart socket): список устройств, проброс портов,
/// загрузка файла и запуск процесса на телефоне.
pub struct Adb {
    addr: String,
}

impl Adb {
    pub fn new(addr: impl Into<String>) -> Self {
        Self { addr: addr.into() }
    }

    pub fn local() -> Self {
        Self::new("127.0.0.1:5037")
    }

    /// Адрес сервера ADB.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    fn open(&self) -> std::io::Result<TcpStream> {
        let addr = self.addr.to_socket_addrs()?.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "плохой адрес ADB-сервера")
        })?;
        let s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(s)
    }

    fn connect(&self) -> Result<TcpStream, String> {
        self.open().map_err(|e| format!("нет ADB-сервера {}: {e}", self.addr))
    }

    /// Шлёт сервис и читает статус `OKAY`/`FAIL<причина>`.
    fn call(s: &mut TcpStream, service: &str) -> Result<(), String> {
        send_msg(s, service).map_err(|e| format!("запрос {service}: {e}"))?;
        let status = read_n(s, 4).map_err(|e| format!("статус {service}: {e}"))?;
        match &status[..] {
            b"OKAY" => Ok(()),
            b"FAIL" => Err(format!("ADB {service}: {}", read_msg(s).unwrap_or_default())),
            _ => Err(format!("ADB {service}: плохой статус")),
        }
    }

    /// Запрос с ответом-телом (`host:devices`).
    fn request(&self, service: &str) -> Result<String, String> {
        let mut s = self.connect()?;
        Self::call(&mut s, service)?;
        read_msg(&mut s).map_err(|e| format!("тело {service}: {e}"))
    }

    /// Запрос без тела, на который сервер отвечает `OKAY`, иногда дважды (проброс портов):
    /// второй `OKAY` читаем, если он пришёл.
    fn command(&self, service: &str) -> Result<(), String> {
        let mut s = self.connect()?;
        Self::call(&mut s, service)?;
        s.set_read_timeout(Some(Duration::from_millis(300))).map_err(|e| e.to_string())?;
        let mut more = [0u8; 4];
        let _ = s.read(&mut more); // второй OKAY (если есть) или таймаут: оба нормальные
        Ok(())
    }

    /// Серийники и состояния всех устройств ADB (`device`, `unauthorized`, `offline`…).
    pub fn device_states(&self) -> Result<Vec<(String, String)>, String> {
        let body = self.request("host:devices")?;
        Ok(body
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?.to_owned(), it.next()?.to_owned()))
            })
            .collect())
    }

    /// Серийники устройств в состоянии `device`.
    pub fn devices(&self) -> Result<Vec<String>, String> {
        Ok(self.device_states()?.into_iter().filter(|(_, st)| st == "device").map(|(s, _)| s).collect())
    }

    /// Проброс `tcp:local -> remote` (`tcp:47631`, `localabstract:pcdock`…).
    pub fn forward_to(&self, serial: &str, local_port: u16, remote: &str) -> Result<(), String> {
        self.command(&format!("host-serial:{serial}:forward:tcp:{local_port};{remote}"))
    }

    /// Проброс `tcp:local -> tcp:remote`.
    pub fn forward(&self, serial: &str, local_port: u16, remote_port: u16) -> Result<(), String> {
        self.forward_to(serial, local_port, &format!("tcp:{remote_port}"))
    }

    /// Снять проброс.
    pub fn kill_forward(&self, serial: &str, local_port: u16) -> Result<(), String> {
        self.command(&format!("host-serial:{serial}:killforward:tcp:{local_port}"))
    }

    /// Открывает на телефоне сервис (`exec:…`) и отдаёт сырой поток: после `OKAY` это канал к процессу.
    pub fn open_service(&self, serial: &str, service: &str) -> Result<TcpStream, String> {
        let mut s = self.connect()?;
        Self::call(&mut s, &format!("host:transport:{serial}"))?;
        Self::call(&mut s, service)?;
        Ok(s)
    }

    /// Кладёт файл на телефон (`cat > путь`). Путь без пробелов и кавычек.
    pub fn push(&self, serial: &str, remote_path: &str, data: &[u8]) -> Result<(), String> {
        let mut w = self.open_push(serial, remote_path)?;
        w.write(data)?;
        w.finish().map(|_| ())
    }

    /// Открывает запись файла на телефоне: данные пишутся кусками, `finish` завершает и сообщает вывод команды.
    pub fn open_push(&self, serial: &str, remote_path: &str) -> Result<PushWriter, String> {
        check_remote_path(remote_path)?;
        let s = self.open_service(serial, &format!("exec:cat > {remote_path}"))?;
        s.set_write_timeout(Some(Duration::from_secs(30))).map_err(|e| e.to_string())?;
        Ok(PushWriter { stream: s, path: remote_path.to_string() })
    }

    /// Открывает чтение файла с телефона (`cat путь`): читайте поток до конца. Ошибки команды приходят в том же
    /// потоке текстом, поэтому проверяйте размер заранее (`file_size`) и после чтения.
    pub fn open_pull(&self, serial: &str, remote_path: &str) -> Result<TcpStream, String> {
        check_remote_path(remote_path)?;
        let s = self.open_service(serial, &format!("exec:cat {remote_path}"))?;
        s.set_read_timeout(Some(Duration::from_secs(30))).map_err(|e| e.to_string())?;
        Ok(s)
    }

    /// Выполняет команду на телефоне и возвращает её вывод (stdout вместе со stderr).
    pub fn exec_text(&self, serial: &str, command: &str) -> Result<String, String> {
        let mut s = self.open_service(serial, &format!("exec:{command}"))?;
        s.set_read_timeout(Some(Duration::from_secs(30))).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => return Err(format!("{command}: {e}")),
            }
            if out.len() > 8 * 1024 * 1024 {
                return Err(format!("{command}: слишком длинный вывод"));
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Размер файла на телефоне в байтах; ошибка, если файла нет.
    pub fn file_size(&self, serial: &str, remote_path: &str) -> Result<u64, String> {
        check_remote_path(remote_path)?;
        let out = self.exec_text(serial, &format!("stat -c %s {remote_path}"))?;
        out.trim().parse::<u64>().map_err(|_| format!("нет файла {remote_path}: {}", out.trim()))
    }

    /// Имена в папке на телефоне (у папок на конце `/`).
    pub fn list_dir(&self, serial: &str, remote_path: &str) -> Result<Vec<String>, String> {
        check_remote_path(remote_path)?;
        let out = self.exec_text(serial, &format!("ls -1p {remote_path}"))?;
        if out.contains("No such file")
            || out.contains("Permission denied")
            || out.contains("Not a directory")
        {
            return Err(format!("{remote_path}: {}", out.trim()));
        }
        Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
    }

    /// Удаляет файл на телефоне.
    pub fn remove_file(&self, serial: &str, remote_path: &str) -> Result<(), String> {
        check_remote_path(remote_path)?;
        let out = self.exec_text(serial, &format!("rm -f {remote_path}"))?;
        if out.trim().is_empty() {
            Ok(())
        } else {
            Err(format!("{remote_path}: {}", out.trim()))
        }
    }

    /// Просит галерею и системные приложения заметить файл (рассылка `MEDIA_SCANNER_SCAN_FILE`; на новых
    /// версиях Android действует не везде).
    pub fn media_scan(&self, serial: &str, remote_path: &str) -> Result<(), String> {
        check_remote_path(remote_path)?;
        self.exec_text(
            serial,
            &format!("am broadcast -a android.intent.action.MEDIA_SCANNER_SCAN_FILE -d file://{remote_path}"),
        )
        .map(|_| ())
    }

    /// Запускает команду на телефоне и держит её поток: пока он открыт, процесс живёт; закрытие потока
    /// (наш выход или обрыв) завершает сервер, который следит за своим stdin.
    pub fn spawn(&self, serial: &str, command: &str) -> Result<TcpStream, String> {
        let s = self.open_service(serial, &format!("exec:{command}"))?;
        s.set_read_timeout(None).map_err(|e| e.to_string())?;
        Ok(s)
    }
}

/// Соединение живое, если за короткое время нет ни конца потока, ни ошибки.
fn is_alive(s: &TcpStream) -> bool {
    if s.set_read_timeout(Some(Duration::from_millis(300))).is_err() {
        return false;
    }
    let mut b = [0u8; 1];
    match s.peek(&mut b) {
        Ok(0) => false,
        Ok(_) => true,
        Err(e) => matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
    }
}

/// Путь на телефоне: абсолютный, без `..`, пробелов и символов, которые оболочка поймёт по-своему.
pub fn check_remote_path(path: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("плохой путь на телефоне {path:?}: {why}"));
    if !path.starts_with('/') {
        return bad("нужен абсолютный путь");
    }
    if path.len() > 512 {
        return bad("слишком длинный");
    }
    if path.split('/').any(|p| p == "..") {
        return bad("«..» не допускается");
    }
    if path.chars().any(|c| c.is_whitespace() || c.is_control() || "'\"\\$`;&|<>*?()[]{}!#~".contains(c)) {
        return bad("пробелы и спецсимволы не поддерживаются");
    }
    Ok(())
}

/// Запись файла на телефон куском за куском (см. [`Adb::open_push`]).
pub struct PushWriter {
    stream: TcpStream,
    path: String,
}

impl PushWriter {
    pub fn write(&mut self, data: &[u8]) -> Result<(), String> {
        self.stream.write_all(data).map_err(|e| format!("загрузка {}: {e}", self.path))
    }

    /// Завершает запись и возвращает вывод команды (пустой, если всё прошло гладко).
    pub fn finish(mut self) -> Result<String, String> {
        self.stream.shutdown(std::net::Shutdown::Write).map_err(|e| e.to_string())?;
        self.stream.set_read_timeout(Some(Duration::from_secs(30))).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => return Err(format!("загрузка {}: {e}", self.path)),
            }
        }
        let text = String::from_utf8_lossy(&out).trim().to_string();
        if text.is_empty() {
            Ok(text)
        } else {
            Err(format!("{}: {text}", self.path))
        }
    }
}

fn send_msg(s: &mut TcpStream, body: &str) -> std::io::Result<()> {
    s.write_all(format!("{:04X}{body}", body.len()).as_bytes())
}

fn read_n(s: &mut TcpStream, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_msg(s: &mut TcpStream) -> std::io::Result<String> {
    let len = read_n(s, 4)?;
    let len = usize::from_str_radix(&String::from_utf8_lossy(&len), 16)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "длина ADB"))?;
    Ok(String::from_utf8_lossy(&read_n(s, len)?).into_owned())
}

/// Связь с одной трубкой по ADB: сессия ядра + TCP через проброс.
/// Насос как в USB-`Dock`, только внизу сокет, а не bulk-эндпоинты.
pub struct AdbLink {
    session: Session,
    adb: Adb,
    serial: String,
    local_port: u16,
    /// Серверный процесс на телефоне (решение 32): APK кладётся и запускается с ПК.
    server: Option<ServerSpec>,
    /// Поток процесса сервера: пока открыт, сервер жив.
    held: Option<TcpStream>,
    stream: Option<TcpStream>,
    buf: Vec<u8>,
    t0: std::time::Instant,
}

/// Где лежит сервер на ПК и как он называется на телефоне.
#[derive(Clone, Debug)]
pub struct ServerSpec {
    /// APK сервера на ПК (`server-adb`).
    pub apk: std::path::PathBuf,
    /// Имя abstract-сокета на телефоне.
    pub socket: String,
}

/// Куда кладётся сервер на телефоне.
pub const REMOTE_APK: &str = "/data/local/tmp/pcdock-server.apk";
/// Главный класс сервера.
pub const SERVER_MAIN: &str = "com.device.server.Main";

impl AdbLink {
    pub fn new(config: Config, serial: impl Into<String>, local_port: u16) -> Self {
        Self {
            session: Session::new(config),
            adb: Adb::local(),
            serial: serial.into(),
            local_port,
            server: None,
            held: None,
            stream: None,
            buf: vec![0u8; 65536],
            t0: std::time::Instant::now(),
        }
    }

    /// Связь через серверный процесс на телефоне (без приложения и диалога записи экрана).
    pub fn with_server(mut self, adb: Adb, server: ServerSpec) -> Self {
        self.adb = adb;
        self.server = Some(server);
        self
    }

    /// Кладёт сервер на телефон, запускает и пробрасывает его сокет. Возвращает порт на ПК.
    fn start_server(&mut self, spec: &ServerSpec) -> Result<(), LinkError> {
        let apk = std::fs::read(&spec.apk)
            .map_err(|e| LinkError::Fatal(format!("нет APK сервера {}: {e}", spec.apk.display())))?;
        self.adb.push(&self.serial, REMOTE_APK, &apk).map_err(LinkError::Gone)?;
        let cmd = format!("CLASSPATH={REMOTE_APK} app_process / {SERVER_MAIN} {}", spec.socket);
        self.held = Some(self.adb.spawn(&self.serial, &cmd).map_err(LinkError::Gone)?);
        self.adb
            .forward_to(&self.serial, self.local_port, &format!("localabstract:{}", spec.socket))
            .map_err(LinkError::Gone)?;
        Ok(())
    }

    /// Конструктор для тестов: готовый сокет вместо проброса.
    #[cfg(test)]
    fn with_stream(session: Session, stream: TcpStream) -> Self {
        Self {
            session,
            adb: Adb::local(),
            serial: "test".into(),
            local_port: 0,
            server: None,
            held: None,
            stream: Some(stream),
            buf: vec![0u8; 65536],
            t0: std::time::Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    /// Отправляет всё, что накопило ядро.
    fn flush(&mut self) -> Result<(), LinkError> {
        while let Some(tx) = self.session.poll_transmit() {
            let s = self.stream.as_mut().ok_or_else(|| LinkError::Gone("нет сокета".into()))?;
            match tx {
                Transmit::Stream(bytes) => {
                    s.write_all(&bytes).map_err(|e| LinkError::Gone(format!("запись: {e}")))?;
                }
                Transmit::Control(req) => {
                    // Control-запросы (HID) по ADB идут тем же каналом:
                    // телефонная сторона выполняет их как HID-события.
                    // Формат: [0xC0][request u8][value u16LE][index u16LE][len u16LE][data].
                    let mut pkt = Vec::with_capacity(8 + req.data.len());
                    pkt.push(0xC0);
                    pkt.push(req.request);
                    pkt.extend_from_slice(&req.value.to_le_bytes());
                    pkt.extend_from_slice(&req.index.to_le_bytes());
                    pkt.extend_from_slice(&(req.data.len() as u16).to_le_bytes());
                    pkt.extend_from_slice(&req.data);
                    s.write_all(&pkt).map_err(|e| LinkError::Gone(format!("запись control: {e}")))?;
                }
            }
        }
        Ok(())
    }
}

impl PhoneLink for AdbLink {
    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn connect(&mut self) -> Result<(), LinkError> {
        if self.stream.is_some() {
            return Ok(());
        }
        let addr = format!("127.0.0.1:{}", self.local_port);
        let spec = self.server.clone().ok_or_else(|| {
            LinkError::Fatal("для ADB нужен APK сервера (`server-adb`): ServerSpec не задан".into())
        })?;
        self.start_server(&spec)?;
        // Серверу нужно время, чтобы подняться и открыть сокет.
        let tries = 25;
        let mut attempt = 0;
        let s = loop {
            attempt += 1;
            let sock_addr = addr
                .to_socket_addrs()
                .map_err(|e| LinkError::Fatal(format!("проброс {addr}: {e}")))?
                .next()
                .ok_or_else(|| LinkError::Fatal(format!("проброс {addr}: нет адреса")))?;
            match TcpStream::connect_timeout(&sock_addr, Duration::from_secs(5)) {
                // Порт проброса принимает соединение всегда, даже если сервер на телефоне ещё не открыл
                // сокет: тогда ADB сразу закрывает его. Живое соединение молчит (сервер ждёт нашего HELLO),
                // мёртвое даёт конец потока.
                Ok(s) if !is_alive(&s) => {
                    if attempt >= tries {
                        self.held = None;
                        return Err(LinkError::Gone("сервер на телефоне не открыл сокет".into()));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Ok(s) => break s,
                Err(e) if attempt >= tries => {
                    self.held = None;
                    return Err(LinkError::Gone(format!("телефон не слушает {addr}: {e}")));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        };
        s.set_read_timeout(Some(IO_TIMEOUT)).map_err(|e| LinkError::Fatal(e.to_string()))?;
        s.set_write_timeout(Some(IO_TIMEOUT)).map_err(|e| LinkError::Fatal(e.to_string()))?;
        self.stream = Some(s);
        self.session.start(self.now_ms());
        self.flush()?;
        Ok(())
    }

    fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
        let r = self.poll_inner();
        if matches!(r, Err(LinkError::Gone(_))) {
            // Канал потерян: следующий `connect` поднимет его заново (и сервер на телефоне тоже).
            self.stream = None;
            self.held = None;
        }
        r
    }

    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError> {
        self.input_inner(cmd)
    }

    fn shutdown(&mut self) {
        self.shutdown_inner()
    }
}

impl AdbLink {
    fn poll_inner(&mut self) -> Result<Vec<Event>, LinkError> {
        self.flush()?;
        let s = self.stream.as_mut().ok_or_else(|| LinkError::Gone("нет сокета".into()))?;
        let n = match s.read(&mut self.buf) {
            Ok(n) => n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                0
            }
            Err(e) => return Err(LinkError::Gone(format!("чтение: {e}"))),
        };
        let now = self.now_ms();
        if n > 0 {
            self.session.handle_input(now, &self.buf[..n]);
        }
        if self.session.next_timeout().is_some_and(|t| now >= t) {
            self.session.handle_timeout(now);
        }
        self.flush()?;
        let mut events = Vec::new();
        while let Some(ev) = self.session.poll_event() {
            events.push(ev);
        }
        Ok(events)
    }

    fn input_inner(&mut self, cmd: &InputCmd) -> Result<(), LinkError> {
        let s = &mut self.session;
        let r = match cmd {
            InputCmd::DeviceEnable { kind, on } => {
                s.device_enable(*kind, *on);
                Ok(())
            }
            InputCmd::Touch { cid, phase, x, y, width, height, pressure } => {
                let mut pt = TouchPoint::new(*cid, *phase, *x, *y);
                pt.width = *width;
                pt.height = *height;
                pt.pressure = *pressure;
                s.touch(pt).map_err(|e| e.to_string())
            }
            InputCmd::CursorMoveAbs { x, y } => s.cursor_move_abs(*x, *y).map_err(|e| e.to_string()),
            InputCmd::CursorMoveRel { dx, dy } => s.cursor_move_rel(*dx, *dy).map_err(|e| e.to_string()),
            InputCmd::CursorResync => {
                s.cursor_resync();
                Ok(())
            }
            InputCmd::MouseButton { button, down } => {
                s.mouse_button(*button, *down).map_err(|e| e.to_string())
            }
            InputCmd::Scroll { ticks } => s.scroll(*ticks).map_err(|e| e.to_string()),
            InputCmd::ScrollH { ticks } => s.scroll_h(*ticks).map_err(|e| e.to_string()),
            InputCmd::Key { usage, down } => s.key(*usage, *down).map_err(|e| e.to_string()),
            InputCmd::ReleaseAll => {
                s.release_all();
                Ok(())
            }
            InputCmd::RequestFrame => {
                s.request_frame();
                Ok(())
            }
            InputCmd::Wake => {
                s.wake();
                Ok(())
            }
            InputCmd::App(req) => {
                s.app_request(req);
                Ok(())
            }
            InputCmd::SetStream { video, audio, max_fps } => {
                s.set_stream_switches(*video, *audio, *max_fps);
                Ok(())
            }
        };
        r.map_err(LinkError::Fatal)
    }

    fn shutdown_inner(&mut self) {
        self.session.close();
        self.stream = None;
        self.held = None; // сервер на телефоне видит закрытый поток и завершается
        let _ = self.adb.kill_forward(&self.serial, self.local_port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcdock_core::input::{MouseButton, TouchPhase};
    use pcdock_core::packet::{Hello, Role};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// Что видел фейковый ADB-сервер: запросы и объём загруженных файлов.
    #[derive(Default)]
    struct Seen {
        services: Vec<String>,
        pushed: Vec<usize>,
        /// Файлы «телефона»: путь → содержимое.
        files: std::collections::HashMap<String, Vec<u8>>,
        /// Соединения запущенного сервера, которые ещё открыты.
        spawned_open: usize,
    }

    /// Фейковый ADB-сервер по настоящему протоколу: `host:devices` с телом, `host:transport` без тела,
    /// затем сервис на том же соединении (`exec:` отдаёт сырой поток), `forward` отвечает двойным OKAY.
    fn fake_adb(devices: &str) -> (String, Arc<Mutex<Seen>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let seen2 = seen.clone();
        let body = devices.to_owned();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(16) {
                let Ok(mut s) = stream else { break };
                let seen = seen2.clone();
                let body = body.clone();
                std::thread::spawn(move || {
                    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    loop {
                        let Ok(len) = read_n(&mut s, 4) else { return };
                        let len = usize::from_str_radix(&String::from_utf8_lossy(&len), 16).unwrap_or(0);
                        let mut svc = vec![0u8; len];
                        if s.read_exact(&mut svc).is_err() {
                            return;
                        }
                        let svc = String::from_utf8_lossy(&svc).into_owned();
                        seen.lock().unwrap().services.push(svc.clone());
                        if svc == "host:devices" {
                            let _ = s.write_all(b"OKAY");
                            let _ = s.write_all(format!("{:04X}{body}", body.len()).as_bytes());
                            return;
                        } else if svc.starts_with("host:transport:") {
                            let _ = s.write_all(b"OKAY");
                        } else if let Some(path) = svc.strip_prefix("exec:cat > ") {
                            let _ = s.write_all(b"OKAY");
                            let mut data = Vec::new();
                            let mut buf = [0u8; 4096];
                            while let Ok(n) = s.read(&mut buf) {
                                if n == 0 {
                                    break;
                                }
                                data.extend_from_slice(&buf[..n]);
                            }
                            let mut st = seen.lock().unwrap();
                            st.pushed.push(data.len());
                            st.files.insert(path.to_string(), data);
                            return;
                        } else if let Some(path) = svc.strip_prefix("exec:cat ") {
                            let _ = s.write_all(b"OKAY");
                            let content = seen.lock().unwrap().files.get(path).cloned();
                            let _ = match content {
                                Some(c) => s.write_all(&c),
                                None => s.write_all(
                                    format!("cat: {path}: No such file or directory\n").as_bytes(),
                                ),
                            };
                            return;
                        } else if let Some(path) = svc.strip_prefix("exec:stat -c %s ") {
                            let _ = s.write_all(b"OKAY");
                            let size = seen.lock().unwrap().files.get(path).map(|c| c.len());
                            let _ = match size {
                                Some(n) => s.write_all(format!("{n}\n").as_bytes()),
                                None => s.write_all(
                                    format!("stat: '{path}': No such file or directory\n").as_bytes(),
                                ),
                            };
                            return;
                        } else if let Some(dir) = svc.strip_prefix("exec:ls -1p ") {
                            let _ = s.write_all(b"OKAY");
                            let prefix = format!("{}/", dir.trim_end_matches('/'));
                            let names: Vec<String> = seen
                                .lock()
                                .unwrap()
                                .files
                                .keys()
                                .filter_map(|k| k.strip_prefix(&prefix).map(String::from))
                                .collect();
                            let _ = if names.is_empty() {
                                s.write_all(format!("ls: {dir}: No such file or directory\n").as_bytes())
                            } else {
                                s.write_all((names.join("\n") + "\n").as_bytes())
                            };
                            return;
                        } else if let Some(path) = svc.strip_prefix("exec:rm -f ") {
                            let _ = s.write_all(b"OKAY");
                            seen.lock().unwrap().files.remove(path);
                            return;
                        } else if svc.starts_with("exec:am broadcast") {
                            let _ = s.write_all(b"OKAYBroadcasting: Intent\n");
                            return;
                        } else if svc.starts_with("exec:") {
                            let _ = s.write_all(b"OKAY");
                            seen.lock().unwrap().spawned_open += 1;
                            s.set_read_timeout(None).unwrap();
                            let mut buf = [0u8; 16];
                            while let Ok(n) = s.read(&mut buf) {
                                if n == 0 {
                                    break;
                                }
                            }
                            seen.lock().unwrap().spawned_open -= 1;
                            return;
                        } else if svc.contains(":forward:") {
                            let _ = s.write_all(b"OKAYOKAY");
                            return;
                        } else {
                            let _ = s.write_all(b"OKAY");
                            return;
                        }
                    }
                });
            }
        });
        (addr, seen)
    }

    #[test]
    fn adb_client_lists_and_forwards() {
        let (addr, seen) = fake_adb("ABC123\tdevice\nXYZ\tunauthorized\n");
        let adb = Adb::new(addr);
        assert_eq!(adb.devices().unwrap(), vec!["ABC123"]);
        assert_eq!(adb.device_states().unwrap().len(), 2);
        adb.forward("ABC123", 45631, 47631).unwrap();
        adb.forward_to("ABC123", 45632, "localabstract:pcdock").unwrap();
        adb.kill_forward("ABC123", 45631).unwrap();
        let got = seen.lock().unwrap().services.join("\n");
        assert!(got.contains("host:devices"), "{got}");
        assert!(got.contains("host-serial:ABC123:forward:tcp:45631;tcp:47631"), "{got}");
        assert!(got.contains("host-serial:ABC123:forward:tcp:45632;localabstract:pcdock"), "{got}");
        assert!(got.contains("host-serial:ABC123:killforward:tcp:45631"), "{got}");
    }

    #[test]
    fn push_sends_the_file_and_rejects_bad_paths() {
        let (addr, seen) = fake_adb("");
        let adb = Adb::new(addr);
        let data = vec![7u8; 100_000];
        adb.push("ABC", "/data/local/tmp/x.apk", &data).unwrap();
        let st = seen.lock().unwrap();
        assert_eq!(st.pushed, vec![100_000]);
        assert!(st.services.contains(&"host:transport:ABC".to_string()));
        assert!(st.services.contains(&"exec:cat > /data/local/tmp/x.apk".to_string()));
        drop(st);
        assert!(adb.push("ABC", "/tmp/a b", &data).is_err());
        assert!(adb.push("ABC", "/tmp/a;rm -rf x", &data).is_err());
    }

    #[test]
    fn files_round_trip_through_push_pull_stat_list_and_remove() {
        let (addr, seen) = fake_adb("");
        let adb = Adb::new(addr);
        let data: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();

        // Запись кусками.
        let mut w = adb.open_push("S", "/sdcard/Pictures/a.bin").unwrap();
        for chunk in data.chunks(7000) {
            w.write(chunk).unwrap();
        }
        assert_eq!(w.finish().unwrap(), "");
        assert_eq!(adb.file_size("S", "/sdcard/Pictures/a.bin").unwrap(), 50_000);

        // Чтение.
        let mut r = adb.open_pull("S", "/sdcard/Pictures/a.bin").unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, data);

        assert_eq!(adb.list_dir("S", "/sdcard/Pictures").unwrap(), vec!["a.bin"]);
        adb.media_scan("S", "/sdcard/Pictures/a.bin").unwrap();
        adb.remove_file("S", "/sdcard/Pictures/a.bin").unwrap();
        assert!(adb.file_size("S", "/sdcard/Pictures/a.bin").is_err());
        assert!(seen.lock().unwrap().files.is_empty());
    }

    #[test]
    fn remote_paths_are_checked() {
        for ok in ["/sdcard/a.png", "/storage/emulated/0/DCIM/x_1-2.jpg", "/data/local/tmp/pcdock-server.apk"]
        {
            assert!(check_remote_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "sdcard/a",
            "/sdcard/../data",
            "/sdcard/a b",
            "/sdcard/a;rm",
            "/sdcard/$HOME",
            "/sdcard/`x`",
            "/sdcard/a\nb",
            "/sdcard/*",
            "",
        ] {
            assert!(check_remote_path(bad).is_err(), "{bad:?}");
        }
        let adb = Adb::new("127.0.0.1:1");
        assert!(adb.open_push("S", "/sdcard/a b").is_err());
        assert!(adb.file_size("S", "../x").is_err());
    }

    #[test]
    fn spawn_keeps_the_server_alive_until_the_stream_is_dropped() {
        let (addr, seen) = fake_adb("");
        let adb = Adb::new(addr);
        let stream = adb.spawn("ABC", "CLASSPATH=/x app_process / com.device.server.Main pcdock").unwrap();
        let t0 = std::time::Instant::now();
        while seen.lock().unwrap().spawned_open == 0 && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(seen.lock().unwrap().spawned_open, 1, "процесс запущен");
        drop(stream);
        while seen.lock().unwrap().spawned_open != 0 && t0.elapsed() < Duration::from_secs(4) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(seen.lock().unwrap().spawned_open, 0, "закрытие потока завершает процесс");
    }

    #[test]
    fn adb_link_pumps_session_over_tcp() {
        // Пара сокетов: один конец у связи, другой играет телефон.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let phone = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_millis(800))).unwrap();
            // Читаем HELLO от хоста, отвечаем своим HELLO.
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0);
            let hb = Hello {
                version_max: 2,
                version_min: 2,
                capabilities: 0,
                role: Role::Device,
                name: "phone".into(),
            }
            .to_packet()
            .encode();
            s.write_all(&hb).unwrap();
            // Держим сокет открытым, пока хост качает ввод: вычитываем всё
            // до закрытия хостом (Ok(0)) или нескольких таймаутов подряд.
            let mut idle = 0;
            loop {
                match s.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => idle = 0,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        idle += 1;
                        if idle >= 10 {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        let mut link = AdbLink::with_stream(Session::new(Config::default()), stream);
        link.session.start(1000);
        // Насос: HELLO ушёл, ответ разобран — сессия активна.
        link.poll().unwrap();
        link.poll().unwrap();
        assert!(link.is_connected());
        // Ввод уходит без ошибок (кнопка мыши при включённой мыши по умолчанию).
        link.input(&InputCmd::MouseButton { button: MouseButton::Left, down: true }).unwrap();
        link.input(&InputCmd::Scroll { ticks: 1.0 }).unwrap();
        link.input(&InputCmd::Touch {
            cid: 0,
            phase: TouchPhase::Down,
            x: 0.5,
            y: 0.5,
            width: None,
            height: None,
            pressure: None,
        })
        .unwrap();
        link.poll().unwrap();
        link.shutdown();
        assert!(!link.is_connected());
        phone.join().unwrap();
    }
}
