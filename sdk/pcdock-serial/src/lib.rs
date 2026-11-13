//! Serial-транспорт (решение 27): микроконтроллер — тупая труба, протокол v2 живёт на ПК.
//! По последовательному порту идут те же байты, что и по USB bulk или ADB; управляющие запросы HID
//! кодируются так же, как в ADB-канале: `[0xC0][request u8][value u16LE][index u16LE][len u16LE][данные]`.
//! Прошивка про протокол ничего не знает: гоняет байты между телефоном и ПК.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use pcdock_core::input::TouchPoint;
use pcdock_core::link::{Discovery, InputCmd, LinkError, PhoneLink, Sighting, Transport};
use pcdock_core::packet::SetStream;
use pcdock_core::session::{Config, Event, Session, Transmit};

/// Скорость по умолчанию. У USB-CDC (ttyACM) настройка скорости ни на что не влияет.
pub const DEFAULT_BAUD: u32 = 921_600;

/// Источник фактов: последовательные порты, похожие на трубу микроконтроллера.
#[derive(Default)]
pub struct SerialDiscovery;

impl Discovery for SerialDiscovery {
    fn scan(&mut self) -> Vec<Sighting> {
        port_names()
            .into_iter()
            .map(|path| Sighting { transport: Transport::Serial, address: path, serial: None, issue: None })
            .collect()
    }
}

#[cfg(unix)]
fn port_names() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/dev") {
        for e in dir.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            // Linux: ttyACM (USB-CDC), ttyUSB (USB-UART); macOS: cu.usbmodem*, cu.usbserial*.
            if n.starts_with("ttyACM")
                || n.starts_with("ttyUSB")
                || n.starts_with("cu.usbmodem")
                || n.starts_with("cu.usbserial")
            {
                out.push(format!("/dev/{n}"));
            }
        }
    }
    out.sort();
    out
}

#[cfg(not(unix))]
fn port_names() -> Vec<String> {
    serialport::available_ports().map(|p| p.into_iter().map(|i| i.port_name).collect()).unwrap_or_default()
}

/// Канал поверх любого потока байтов (последовательный порт, пара сокетов в тестах).
pub struct StreamLink<S: Read + Write + Send> {
    session: Session,
    /// Как открыть поток заново (после обрыва).
    open: Box<dyn FnMut() -> Result<S, String> + Send>,
    stream: Option<S>,
    buf: Vec<u8>,
    t0: Instant,
}

/// Канал к последовательному порту.
pub type SerialLink = StreamLink<Box<dyn serialport::SerialPort>>;

impl SerialLink {
    pub fn open(config: Config, path: impl Into<String>, baud: u32) -> Self {
        let path = path.into();
        StreamLink::new(
            config,
            Box::new(move || {
                serialport::new(&path, baud)
                    .timeout(Duration::from_millis(100))
                    .open()
                    .map_err(|e| format!("{path}: {e}"))
            }),
        )
    }
}

impl<S: Read + Write + Send> StreamLink<S> {
    pub fn new(config: Config, open: Box<dyn FnMut() -> Result<S, String> + Send>) -> Self {
        Self { session: Session::new(config), open, stream: None, buf: vec![0u8; 65536], t0: Instant::now() }
    }

    fn now_ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }

    fn flush(&mut self) -> Result<(), LinkError> {
        while let Some(tx) = self.session.poll_transmit() {
            let s = self.stream.as_mut().ok_or_else(|| LinkError::Gone("порт закрыт".into()))?;
            let bytes = match tx {
                Transmit::Stream(bytes) => bytes,
                Transmit::Control(req) => {
                    let mut pkt = Vec::with_capacity(8 + req.data.len());
                    pkt.push(0xC0);
                    pkt.push(req.request);
                    pkt.extend_from_slice(&req.value.to_le_bytes());
                    pkt.extend_from_slice(&req.index.to_le_bytes());
                    pkt.extend_from_slice(&(req.data.len() as u16).to_le_bytes());
                    pkt.extend_from_slice(&req.data);
                    pkt
                }
            };
            s.write_all(&bytes).map_err(|e| LinkError::Gone(format!("запись: {e}")))?;
        }
        Ok(())
    }

    fn lost(&mut self, why: String) -> LinkError {
        self.stream = None;
        LinkError::Gone(why)
    }
}

impl<S: Read + Write + Send> PhoneLink for StreamLink<S> {
    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn connect(&mut self) -> Result<(), LinkError> {
        if self.stream.is_some() {
            return Ok(());
        }
        self.stream = Some((self.open)().map_err(LinkError::Gone)?);
        self.session.start(self.now_ms());
        self.flush().map_err(|e| self.lost(e.to_string()))
    }

    fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
        if let Err(e) = self.flush() {
            return Err(self.lost(e.to_string()));
        }
        let s = self.stream.as_mut().ok_or_else(|| LinkError::Gone("порт закрыт".into()))?;
        let n = match s.read(&mut self.buf) {
            Ok(0) => return Err(self.lost("порт закрылся".into())),
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => 0,
            Err(e) => return Err(self.lost(format!("чтение: {e}"))),
        };
        let now = self.now_ms();
        if n > 0 {
            self.session.handle_input(now, &self.buf[..n]);
        }
        if self.session.next_timeout().is_some_and(|t| now >= t) {
            self.session.handle_timeout(now);
        }
        if let Err(e) = self.flush() {
            return Err(self.lost(e.to_string()));
        }
        let mut events = Vec::new();
        while let Some(ev) = self.session.poll_event() {
            events.push(ev);
        }
        Ok(events)
    }

    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError> {
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
                s.set_stream(SetStream {
                    video: *video,
                    audio: *audio,
                    max_fps: *max_fps,
                    ..SetStream::default()
                });
                Ok(())
            }
        };
        r.map_err(LinkError::Fatal)
    }

    fn shutdown(&mut self) {
        self.session.close();
        self.stream = None;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use pcdock_core::packet::{Hello, Role};
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};

    /// Пара сокетов: один конец у канала, другой играет прошивку и телефон.
    fn link_with_peer() -> (StreamLink<UnixStream>, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let slot = Arc::new(Mutex::new(Some(a)));
        let link = StreamLink::new(
            Config::default(),
            Box::new(move || slot.lock().unwrap().take().ok_or("занято".into())),
        );
        (link, b)
    }

    fn phone_hello() -> Vec<u8> {
        Hello { version_max: 2, version_min: 2, capabilities: 0, role: Role::Device, name: "mcu".into() }
            .to_packet()
            .encode()
    }

    #[test]
    fn pumps_hello_and_events_over_a_stream() {
        let (mut link, mut peer) = link_with_peer();
        peer.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        link.connect().unwrap();
        assert!(link.is_connected());
        // ПК первым шлёт HELLO.
        let mut buf = [0u8; 1024];
        let n = peer.read(&mut buf).unwrap();
        assert!(n >= 8 && &buf[..2] == b"PD", "пакет протокола");
        peer.write_all(&phone_hello()).unwrap();
        let mut got = Vec::new();
        for _ in 0..20 {
            got.extend(link.poll().unwrap());
            if got.iter().any(|e| matches!(e, Event::Connected { .. })) {
                break;
            }
        }
        assert!(
            got.iter().any(|e| matches!(e, Event::Connected { peer_name, .. } if peer_name == "mcu")),
            "{got:?}"
        );
    }

    #[test]
    fn control_requests_are_framed_like_the_adb_channel() {
        let (mut link, mut peer) = link_with_peer();
        peer.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        link.connect().unwrap();
        peer.write_all(&phone_hello()).unwrap();
        for _ in 0..10 {
            link.poll().unwrap();
        }
        link.input(&InputCmd::Scroll { ticks: 1.0 }).unwrap();
        link.poll().unwrap();
        let mut all = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = peer.read(&mut buf) {
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buf[..n]);
        }
        assert!(all.contains(&0xC0), "управляющие запросы идут с шапкой 0xC0");
    }

    #[test]
    fn closed_port_is_reported_and_reopened() {
        let (mut link, peer) = link_with_peer();
        link.connect().unwrap();
        drop(peer);
        let r = (0..5).map(|_| link.poll()).find(|r| r.is_err());
        assert!(matches!(r, Some(Err(LinkError::Gone(_)))), "{r:?}");
        assert!(!link.is_connected(), "после обрыва порт считается закрытым");
    }
}
