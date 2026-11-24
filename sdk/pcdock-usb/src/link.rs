//! `UsbLink`: [`Dock`](crate::Dock) как транспорт для фермы.

use pcdock_core::input::TouchPoint;
use pcdock_core::link::{InputCmd, LinkError, PhoneLink};
use pcdock_core::session::Event;

/// USB-связь поверх готового `Dock`: одна трубка, ретраи снаружи в воркере.
pub struct UsbLink {
    dock: crate::Dock,
}

impl UsbLink {
    pub fn new(config: pcdock_core::session::Config, settings: crate::Settings) -> Result<Self, String> {
        crate::Dock::new(config, settings).map(|dock| Self { dock }).map_err(|e| e.to_string())
    }
}

impl PhoneLink for UsbLink {
    fn is_connected(&self) -> bool {
        self.dock.is_connected()
    }

    fn connect(&mut self) -> Result<(), LinkError> {
        self.dock.connect().map_err(|e| classify_usb(&e))
    }

    fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
        let mut events = self.dock.poll().map_err(|e| classify_usb(&e))?;
        for w in self.dock.take_warnings() {
            events.push(Event::Status(pcdock_core::packet::Status { code: 0, detail: w }));
        }
        Ok(events)
    }

    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError> {
        let s = self.dock.session();
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
        r.map_err(|e| LinkError::Fatal(e.to_string()))
    }

    fn shutdown(&mut self) {
        self.dock.shutdown();
    }
}

fn classify_usb(e: &crate::Error) -> LinkError {
    match e {
        // Нет прав или драйвера: повторять бессмысленно, человеку нужно подсказать, что сделать.
        crate::Error::Usb(rusb::Error::Access | rusb::Error::NotSupported) => {
            LinkError::Blocked(pcdock_core::link::PhoneState::NeedsAccess, e.to_string())
        }
        crate::Error::Usb(_) | crate::Error::NoDevice | crate::Error::HandshakeTimeout => {
            LinkError::Gone(e.to_string())
        }
        _ => LinkError::Fatal(e.to_string()),
    }
}

/// Источник фактов о телефонах на USB для фермы. Серийник читается один раз на порт (для этого телефон
/// приходится открыть; на macOS это может мешать adb) и нужен, чтобы узнать тот же телефон по ADB.
pub struct UsbDiscovery {
    ctx: rusb::Context,
    vendor_ids: Vec<u16>,
    /// Порт → (серийник, сколько раз пробовали читать). Неудачное чтение повторяется не больше трёх раз;
    /// отсутствие прав не считается попыткой: после правки прав телефон подхватится сам.
    serials: std::collections::HashMap<crate::UsbPort, (Option<String>, u8)>,
    /// События подключения и отключения от libusb (если платформа их даёт).
    hotplug: Option<Hotplug>,
}

/// Подписка на события libusb. Поток обработки живёт, пока жива подписка.
struct Hotplug {
    changed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Держит регистрацию колбэка.
    _registration: rusb::Registration<rusb::Context>,
}

struct HotplugHandler {
    vendor_ids: Vec<u16>,
    changed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl HotplugHandler {
    fn note<T: rusb::UsbContext>(&self, device: &rusb::Device<T>) {
        if let Ok(desc) = device.device_descriptor() {
            if self.vendor_ids.contains(&desc.vendor_id()) {
                self.changed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

impl<T: rusb::UsbContext> rusb::Hotplug<T> for HotplugHandler {
    fn device_arrived(&mut self, device: rusb::Device<T>) {
        self.note(&device);
    }

    fn device_left(&mut self, device: rusb::Device<T>) {
        self.note(&device);
    }
}

impl Drop for Hotplug {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl UsbDiscovery {
    pub fn new(vendor_ids: Vec<u16>) -> Result<Self, String> {
        use rusb::UsbContext;
        let ctx = rusb::Context::new().map_err(|e| e.to_string())?;
        let hotplug = if rusb::has_hotplug() {
            let changed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let handler = HotplugHandler { vendor_ids: vendor_ids.clone(), changed: changed.clone() };
            match rusb::HotplugBuilder::new().enumerate(false).register(&ctx, Box::new(handler)) {
                Ok(registration) => {
                    let (ctx2, stop2) = (ctx.clone(), stop.clone());
                    let thread = std::thread::Builder::new()
                        .name("usb-hotplug".into())
                        .spawn(move || {
                            while !stop2.load(std::sync::atomic::Ordering::SeqCst) {
                                let _ = ctx2.handle_events(Some(std::time::Duration::from_millis(250)));
                            }
                        })
                        .ok();
                    Some(Hotplug { changed, stop, thread, _registration: registration })
                }
                Err(_) => None,
            }
        } else {
            None
        };
        Ok(Self { ctx, vendor_ids, serials: Default::default(), hotplug })
    }

    /// Работают ли события подключения (иначе остаётся только опрос по таймеру).
    pub fn has_hotplug(&self) -> bool {
        self.hotplug.is_some()
    }
}

impl pcdock_core::link::Discovery for UsbDiscovery {
    fn changed(&mut self) -> bool {
        self.hotplug.as_ref().is_some_and(|h| h.changed.swap(false, std::sync::atomic::Ordering::SeqCst))
    }

    fn scan(&mut self) -> Vec<pcdock_core::link::Sighting> {
        use pcdock_core::link::{Sighting, Transport};
        let Ok(found) = crate::scan(&self.ctx, &self.vendor_ids) else { return Vec::new() };
        // Порт, который исчез, забываем: в него может воткнуться другой телефон.
        self.serials.retain(|port, _| found.iter().any(|(_, info)| &info.port == port));
        let mut out = Vec::new();
        for (device, info) in &found {
            let entry = self.serials.entry(info.port.clone()).or_insert((None, 0));
            let mut issue = None;
            if entry.0.is_none() && entry.1 < 3 {
                let (serial, problem) = crate::probe_serial(device, std::time::Duration::from_millis(500));
                issue = problem;
                if issue.is_none() {
                    entry.1 += 1;
                }
                entry.0 = serial;
            } else if entry.0.is_none() {
                // Серийник не читается, но права проверим: вдруг их только что выдали (или отозвали).
                issue = crate::probe_serial(device, std::time::Duration::from_millis(500)).1;
            }
            out.push(Sighting {
                transport: Transport::Usb,
                address: info.port.to_string(),
                serial: entry.0.clone(),
                issue,
            });
        }
        out
    }
}
