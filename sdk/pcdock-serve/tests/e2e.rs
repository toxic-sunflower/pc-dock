//! Сквозные тесты связи: демон с фейковой трубкой, клиенты на TCP, WebSocket и локальной трубе:
//! приветствие и версия, пароль, номера запросов, подписки с условиями, спрос на видео, снимок кадра,
//! изоляция медленного клиента.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pcdock_core::link::{InputCmd, LinkError, PhoneLink};
use pcdock_core::session::Event;
use pcdock_farm::Farm;
use pcdock_serve::protocol::{decode_video, ClientMsg, ServerMsg, PKG_JSON, PKG_VIDEO};
use pcdock_serve::Serve;
use serde_json::Value;

struct FakeLink {
    connected: bool,
    events: Arc<Mutex<VecDeque<Event>>>,
    inputs: Arc<Mutex<Vec<String>>>,
}

impl PhoneLink for FakeLink {
    fn is_connected(&self) -> bool {
        self.connected
    }
    fn connect(&mut self) -> Result<(), LinkError> {
        self.connected = true;
        Ok(())
    }
    fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
        // Чуть притормаживаем busy-loop воркера, чтобы не жечь CPU.
        std::thread::sleep(Duration::from_millis(2));
        let mut q = self.events.lock().unwrap();
        let n = q.len().min(64);
        Ok(q.drain(..n).collect())
    }
    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError> {
        self.inputs.lock().unwrap().push(format!("{cmd:?}"));
        Ok(())
    }
    fn shutdown(&mut self) {
        self.connected = false;
    }
}

fn video(pts: u64) -> Event {
    Event::Video(pcdock_core::packet::VideoFrame {
        codec: 1,
        key_frame: true,
        pts_us: pts,
        data: vec![7, 7, 7],
    })
}

/// Фейковый телефон: общая очередь событий и журнал команд ввода.
struct Phone {
    events: Arc<Mutex<VecDeque<Event>>>,
    inputs: Arc<Mutex<Vec<String>>>,
}

impl Phone {
    fn push(&self, ev: Event) {
        self.events.lock().unwrap().push_back(ev);
    }

    fn log(&self) -> String {
        self.inputs.lock().unwrap().join("\n")
    }

    /// Последняя команда `SetStream`, которую получил телефон.
    fn last_stream(&self) -> Option<String> {
        self.inputs.lock().unwrap().iter().rev().find(|s| s.contains("SetStream")).cloned()
    }

    fn wait_stream(&self, want: &str) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            if self.last_stream().is_some_and(|s| s.contains(want)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

fn farm_with_phone(id: u32) -> (Farm, Phone) {
    let phone =
        Phone { events: Arc::new(Mutex::new(VecDeque::new())), inputs: Arc::new(Mutex::new(Vec::new())) };
    let mut farm = Farm::new();
    farm.add_phone(
        id,
        Box::new(FakeLink { connected: false, events: phone.events.clone(), inputs: phone.inputs.clone() }),
    )
    .unwrap();
    // Телефон «подключился»: сервер должен сообщить ему, что от него нужно.
    phone.push(Event::Connected { version: 2, capabilities: 0, peer_name: "fake".into() });
    (farm, phone)
}

/// TCP-клиент протокола приложения.
struct Conn {
    sock: TcpStream,
}

impl Conn {
    fn open(addr: std::net::SocketAddr) -> Self {
        let sock = TcpStream::connect(addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Self { sock }
    }

    fn send_raw(&mut self, json: &str) {
        let mut env = Vec::new();
        env.extend_from_slice(&((1 + json.len()) as u32).to_le_bytes());
        env.push(PKG_JSON);
        env.extend_from_slice(json.as_bytes());
        self.sock.write_all(&env).unwrap();
    }

    fn send(&mut self, msg: &ClientMsg) {
        self.send_raw(&serde_json::to_string(msg).unwrap());
    }

    fn recv(&mut self) -> (u8, Vec<u8>) {
        let mut len = [0u8; 4];
        self.sock.read_exact(&mut len).unwrap();
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        self.sock.read_exact(&mut buf).unwrap();
        (buf[0], buf[1..].to_vec())
    }

    /// Следующее JSON-сообщение (бинарные пропускает).
    fn json(&mut self) -> Value {
        loop {
            let (kind, body) = self.recv();
            if kind == PKG_JSON {
                return serde_json::from_slice(&body).unwrap();
            }
        }
    }

    /// Следующее JSON-сообщение сервера как `ServerMsg`, не считая рассылки о состояниях телефонов
    /// (они приходят всем и могут вклиниться между ответами).
    fn msg(&mut self) -> ServerMsg {
        loop {
            let m: ServerMsg = serde_json::from_value(self.json()).unwrap();
            if !matches!(m, ServerMsg::State { .. } | ServerMsg::Appeared { .. } | ServerMsg::Lost { .. }) {
                return m;
            }
        }
    }

    /// Следующий кадр видео: (телефон, номер кадра, метка времени).
    fn frame(&mut self) -> (u32, u64, u64) {
        loop {
            let (kind, body) = self.recv();
            if kind == PKG_VIDEO {
                let (phone, seq, _, _, pts, jpeg) = decode_video(&body).unwrap();
                assert_eq!(jpeg, &[7, 7, 7]);
                return (phone, seq, pts);
            }
        }
    }

    /// Читает сообщения до паузы и считает кадры видео.
    fn count_frames_until_quiet(&mut self, quiet: Duration) -> usize {
        self.sock.set_read_timeout(Some(quiet)).unwrap();
        let mut n = 0;
        loop {
            let mut len = [0u8; 4];
            if self.sock.read_exact(&mut len).is_err() {
                break;
            }
            let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
            if self.sock.read_exact(&mut buf).is_err() {
                break;
            }
            if buf[0] == PKG_VIDEO {
                n += 1;
            }
        }
        self.sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        n
    }

    /// Первое сообщение: приветствие с версией протокола.
    fn expect_hello(&mut self) {
        match self.msg() {
            ServerMsg::Hello { protocol, server } => {
                assert_eq!(protocol, pcdock_serve::PROTOCOL_VERSION);
                assert!(server.starts_with("pcdockd/"), "{server}");
            }
            other => panic!("ждали Hello, пришло {other:?}"),
        }
    }
}

#[test]
fn tcp_hello_watch_input_and_frame() {
    let (farm, phone) = farm_with_phone(7);
    let serve = Serve::new(farm);
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    let mut c = Conn::open(addr);

    c.expect_hello();
    c.send(&ClientMsg::List);
    match c.msg() {
        ServerMsg::Phones { phones, details } => {
            assert_eq!(phones, vec![7]);
            // Подробности: состояние, путь (у добавленного вручную нет), имя после рукопожатия.
            assert_eq!(details.len(), 1);
            assert_eq!(details[0]["phone"], 7);
            assert!(details[0]["state"].is_string());
        }
        other => panic!("{other:?}"),
    }

    c.send_raw(r#"{"op":"Watch","phone":7}"#);
    assert_eq!(c.msg(), ServerMsg::Watched { phone: 7, on: true });

    // Кадры кладём после подписки: до неё демон их не пропускает клиентам.
    phone.push(video(10));

    c.send(&ClientMsg::Scroll { phone: 7, ticks: 3.0 });
    std::thread::sleep(Duration::from_millis(300));
    assert!(phone.log().contains("Scroll"), "{}", phone.log());

    // Номера кадров по порядку; клиент, который успевает читать, получает каждый кадр.
    assert_eq!(c.frame(), (7, 1, 10));
    phone.push(video(11));
    assert_eq!(c.frame(), (7, 2, 11));
}

#[test]
fn request_ids_are_echoed_in_replies() {
    let (farm, _phone) = farm_with_phone(3);
    let serve = Serve::new(farm);
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    let mut c = Conn::open(addr);
    c.expect_hello();

    c.send_raw(r#"{"op":"List","id":41}"#);
    let v = c.json();
    assert_eq!(v["ev"], "Phones");
    assert_eq!(v["id"], 41);

    // Команда без собственного ответа: `Ok` с тем же номером.
    c.send_raw(r#"{"op":"Wake","phone":3,"id":42}"#);
    let v = c.json();
    assert_eq!((v["ev"].as_str(), v["id"].as_u64()), (Some("Ok"), Some(42)));

    // Ошибка тоже несёт номер запроса.
    c.send_raw(r#"{"op":"Wake","phone":99,"id":43}"#);
    let v = c.json();
    assert_eq!((v["ev"].as_str(), v["id"].as_u64()), (Some("Error"), Some(43)));

    // Выбор пути: неизвестное имя и телефон без обнаружения дают ошибку с номером запроса.
    c.send_raw(r#"{"op":"SetTransport","phone":3,"transport":"wifi","id":44}"#);
    let v = c.json();
    assert_eq!((v["ev"].as_str(), v["id"].as_u64()), (Some("Error"), Some(44)));
    c.send_raw(r#"{"op":"SetTransport","phone":3,"transport":"adb","id":45}"#);
    let v = c.json();
    assert_eq!((v["ev"].as_str(), v["id"].as_u64()), (Some("Error"), Some(45)));

    // Без номера всё как раньше: ответа `Ok` нет, ошибка без `id`.
    c.send_raw(r#"{"op":"Wake","phone":99}"#);
    let v = c.json();
    assert_eq!(v["ev"], "Error");
    assert!(v.get("id").is_none());
}

#[test]
fn ws_watch_and_typed_binary_frame() {
    let (farm, phone) = farm_with_phone(9);
    let serve = Serve::new(farm);
    let addr = serve.run_ws("127.0.0.1:0").unwrap();
    let (mut ws, _) = tungstenite::connect(format!("ws://{addr}/")).map_err(|e| format!("{e:?}")).unwrap();

    fn next_json<S: Read + Write>(ws: &mut tungstenite::WebSocket<S>) -> ServerMsg {
        loop {
            if let tungstenite::Message::Text(t) = ws.read().unwrap() {
                let m: ServerMsg = serde_json::from_str(&t).unwrap();
                if !matches!(m, ServerMsg::State { .. } | ServerMsg::Appeared { .. } | ServerMsg::Lost { .. })
                {
                    return m;
                }
            }
        }
    }
    assert!(matches!(next_json(&mut ws), ServerMsg::Hello { .. }));
    ws.send(tungstenite::Message::Text(r#"{"op":"List"}"#.into())).unwrap();
    assert!(matches!(next_json(&mut ws), ServerMsg::Phones { phones, .. } if phones == vec![9]));
    ws.send(tungstenite::Message::Text(r#"{"op":"Watch","phone":9}"#.into())).unwrap();
    assert_eq!(next_json(&mut ws), ServerMsg::Watched { phone: 9, on: true });

    phone.push(video(20));
    let body = loop {
        if let tungstenite::Message::Binary(b) = ws.read().unwrap() {
            break b.to_vec();
        }
    };
    // Первый байт бинарного фрейма: тип (1 видео).
    assert_eq!(body[0], 1);
    let (p, seq, codec, key, pts, jpeg) = decode_video(&body[1..]).unwrap();
    assert_eq!((p, seq, codec, key, pts), (9, 1, 1, true, 20));
    assert_eq!(jpeg, &[7, 7, 7]);
}

#[test]
fn local_watch_and_frame() {
    use interprocess::local_socket::{
        traits::Stream as _, GenericNamespaced, Stream as LocalStream, ToNsName,
    };

    let (farm, phone) = farm_with_phone(5);
    let serve = Serve::new(farm);
    let name = format!("pcdock-test-{}", std::process::id());
    serve.run_local(&name).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let ns = Box::leak(name.into_boxed_str()) as &'static str;
    let mut sock = LocalStream::connect(ns.to_ns_name::<GenericNamespaced>().unwrap()).unwrap();
    let recv = |sock: &mut LocalStream| -> (u8, Vec<u8>) {
        let mut len = [0u8; 4];
        sock.read_exact(&mut len).unwrap();
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        sock.read_exact(&mut buf).unwrap();
        (buf[0], buf[1..].to_vec())
    };
    let hello = recv(&mut sock);
    assert_eq!(hello.0, PKG_JSON);
    assert!(String::from_utf8_lossy(&hello.1).contains("Hello"));

    let body = br#"{"op":"Watch","phone":5}"#;
    let mut env = ((1 + body.len()) as u32).to_le_bytes().to_vec();
    env.push(PKG_JSON);
    env.extend_from_slice(body);
    sock.write_all(&env).unwrap();
    let (kind, reply) = recv(&mut sock);
    assert_eq!(kind, PKG_JSON);
    assert_eq!(
        serde_json::from_slice::<ServerMsg>(&reply).unwrap(),
        ServerMsg::Watched { phone: 5, on: true }
    );

    phone.push(video(30));
    let (kind, body) = recv(&mut sock);
    assert_eq!(kind, PKG_VIDEO);
    let (p, seq, _, _, pts, _) = decode_video(&body).unwrap();
    assert_eq!((p, seq, pts), (5, 1, 30));
}

#[test]
fn token_is_required_before_anything_else() {
    let serve = Serve::with_token(Farm::new(), Some("s3cret".into()));
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    let mut c = Conn::open(addr);
    c.expect_hello(); // приветствие идёт до пароля

    // Без пароля команды не исполняются.
    c.send(&ClientMsg::List);
    assert!(matches!(c.msg(), ServerMsg::Error { .. }));

    // Неверный пароль отклоняется.
    c.send(&ClientMsg::Auth { token: "wrong".into() });
    assert!(matches!(c.msg(), ServerMsg::Error { .. }));

    // Верный открывает доступ.
    c.send(&ClientMsg::Auth { token: "s3cret".into() });
    assert_eq!(c.msg(), ServerMsg::Authed);
    c.send(&ClientMsg::List);
    assert!(matches!(c.msg(), ServerMsg::Phones { phones, .. } if phones.is_empty()));
}

#[test]
fn per_client_fps_limits_only_that_client() {
    let (farm, phone) = farm_with_phone(1);
    let serve = Serve::new(farm);
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    let mut fast = Conn::open(addr);
    let mut slow = Conn::open(addr);
    for c in [&mut fast, &mut slow] {
        c.expect_hello();
    }
    fast.send_raw(r#"{"op":"Watch","phone":1}"#);
    slow.send_raw(r#"{"op":"Watch","phone":1,"fps":5}"#);
    assert!(matches!(fast.msg(), ServerMsg::Watched { .. }));
    assert!(matches!(slow.msg(), ServerMsg::Watched { .. }));

    // Телефон шлёт кадры заметно чаще 5 в секунду на протяжении секунды.
    for i in 0..400 {
        phone.push(video(i));
    }
    let n_fast = fast.count_frames_until_quiet(Duration::from_millis(700));
    let n_slow = slow.count_frames_until_quiet(Duration::from_millis(700));
    assert!(n_slow >= 1, "ограниченный клиент кадры получает");
    assert!(n_slow <= 8, "не больше ~5 кадров в секунду: {n_slow}");
    assert!(n_fast > n_slow * 3, "клиент без ограничения получает заметно больше: {n_fast} против {n_slow}");
}

#[test]
fn on_change_skips_identical_frames() {
    let (farm, phone) = farm_with_phone(1);
    let serve = Serve::new(farm);
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    let mut c = Conn::open(addr);
    c.expect_hello();
    c.send_raw(r#"{"op":"Watch","phone":1,"on_change":true}"#);
    assert!(matches!(c.msg(), ServerMsg::Watched { .. }));
    // Одинаковые кадры (данные те же), потом другой; кадры идут с паузами, чтобы слот не перезаписывался.
    let mut n = 0;
    for i in 0..5 {
        phone.push(video(i));
        n += c.count_frames_until_quiet(Duration::from_millis(80));
    }
    phone.push(Event::Video(pcdock_core::packet::VideoFrame {
        codec: 1,
        key_frame: false,
        pts_us: 99,
        data: vec![9, 9, 9],
    }));
    n += c.count_frames_until_quiet(Duration::from_millis(300));
    assert_eq!(n, 2, "первый кадр и отличающийся");
}

#[test]
fn phone_streams_only_while_somebody_watches_at_the_highest_rate() {
    let (farm, phone) = farm_with_phone(1);
    let serve = Serve::new(farm);
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    // Телефон подключился, смотрящих нет: видео выключено.
    assert!(phone.wait_stream("video: false"), "{}", phone.log());

    let mut ui = Conn::open(addr);
    let mut cv = Conn::open(addr);
    for c in [&mut ui, &mut cv] {
        c.expect_hello();
    }
    cv.send_raw(r#"{"op":"Watch","phone":1,"fps":2}"#);
    assert!(matches!(cv.msg(), ServerMsg::Watched { .. }));
    assert!(phone.wait_stream("video: true, audio: false, max_fps: 2"), "{}", phone.log());

    // Подключился UI со всеми кадрами: телефон снимает без ограничения.
    ui.send_raw(r#"{"op":"Watch","phone":1,"audio":true}"#);
    assert!(matches!(ui.msg(), ServerMsg::Watched { .. }));
    assert!(phone.wait_stream("video: true, audio: true, max_fps: 0"), "{}", phone.log());

    // UI ушёл: остался CV на 2 кадра в секунду, телефон снова ограничен; звук не нужен.
    ui.send_raw(r#"{"op":"Unwatch","phone":1}"#);
    assert!(matches!(ui.msg(), ServerMsg::Watched { on: false, .. }));
    assert!(phone.wait_stream("video: true, audio: false, max_fps: 2"), "{}", phone.log());

    // Закрытое соединение снимает подписку: смотрящих нет, видео выключается.
    drop(cv);
    assert!(phone.wait_stream("video: false"), "{}", phone.log());
}

#[test]
fn unwatch_by_one_client_does_not_stop_video_for_another() {
    let (farm, phone) = farm_with_phone(1);
    let serve = Serve::new(farm);
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    let mut a = Conn::open(addr);
    let mut b = Conn::open(addr);
    for c in [&mut a, &mut b] {
        c.expect_hello();
        c.send_raw(r#"{"op":"Watch","phone":1}"#);
        assert!(matches!(c.msg(), ServerMsg::Watched { on: true, .. }));
    }
    a.send_raw(r#"{"op":"Unwatch","phone":1}"#);
    assert!(matches!(a.msg(), ServerMsg::Watched { on: false, .. }));
    phone.push(video(5));
    assert_eq!(b.frame().2, 5, "второй клиент по-прежнему получает кадры");
}

#[test]
fn snapshot_returns_the_last_frame_to_the_requester_only() {
    let (farm, phone) = farm_with_phone(1);
    let serve = Serve::new(farm);
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    let mut watcher = Conn::open(addr);
    watcher.expect_hello();
    watcher.send_raw(r#"{"op":"Watch","phone":1}"#);
    assert!(matches!(watcher.msg(), ServerMsg::Watched { .. }));
    phone.push(video(77));
    assert_eq!(watcher.frame().2, 77);

    let mut other = Conn::open(addr);
    other.expect_hello();
    // Снимок без подписки и без неё же не мешает: приходит один кадр, запрос с номером получает ответ.
    other.send_raw(r#"{"op":"Snapshot","phone":1,"id":8}"#);
    let mut got_frame = false;
    let mut got_ok = false;
    for _ in 0..2 {
        let (kind, body) = other.recv();
        if kind == PKG_VIDEO {
            assert_eq!(decode_video(&body).unwrap().4, 77);
            got_frame = true;
        } else {
            let v: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!((v["ev"].as_str(), v["id"].as_u64()), (Some("Ok"), Some(8)));
            got_ok = true;
        }
    }
    assert!(got_frame && got_ok);

    // Телефона без кадров: ошибка.
    other.send_raw(r#"{"op":"Snapshot","phone":2,"id":9}"#);
    let v = other.json();
    assert_eq!((v["ev"].as_str(), v["id"].as_u64()), (Some("Error"), Some(9)));
}

#[test]
fn a_client_that_never_reads_cannot_stall_the_others() {
    let (farm, phone) = farm_with_phone(1);
    let serve = Serve::new(farm);
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    let mut stuck = Conn::open(addr);
    let mut live = Conn::open(addr);
    for c in [&mut stuck, &mut live] {
        c.expect_hello();
        c.send_raw(r#"{"op":"Watch","phone":1}"#);
    }
    assert!(matches!(live.msg(), ServerMsg::Watched { .. }));
    // `stuck` дальше ничего не читает. Событий столько, что не помещается ни в сокет, ни в ящик.
    let filler = "x".repeat(4000);
    for i in 0..6_000 {
        phone.push(Event::Status(pcdock_core::packet::Status { code: 0, detail: format!("{i}{filler}") }));
    }
    phone.push(Event::Status(pcdock_core::packet::Status { code: 0, detail: "LAST".into() }));

    // Живой клиент получает всё до последнего события, поток раздачи не встал.
    let t0 = Instant::now();
    loop {
        let v = live.json();
        if v["ev"] == "Status" && v["detail"] == "LAST" {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "раздача встала");
    }

    // Не читавший клиент отключён сервером.
    stuck.sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = vec![0u8; 1 << 16];
    let t1 = Instant::now();
    loop {
        match stuck.sock.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        assert!(t1.elapsed() < Duration::from_secs(10), "сервер не отключил клиента");
    }
}

#[test]
fn app_requests_reach_the_phone_and_replies_go_to_the_requester_without_watch() {
    let (farm, phone) = farm_with_phone(5);
    let serve = Serve::new(farm);
    let addr = serve.run_tcp("127.0.0.1:0").unwrap();
    let mut asker = Conn::open(addr);
    asker.expect_hello();
    let mut other = Conn::open(addr);
    other.expect_hello();

    asker.send_raw(r#"{"op":"LaunchApp","phone":5,"request":7,"package":"com.example.app","id":1}"#);
    assert_eq!(asker.json()["ev"], "Ok");
    asker.send_raw(r#"{"op":"UiDump","phone":5,"request":8}"#);
    asker.send_raw(r#"{"op":"ExitInfo","phone":5,"request":9,"since_ms":100,"package":"com.example.app"}"#);
    asker.send_raw(r#"{"op":"IpCheck","phone":5,"request":10,"url":"https://example.com/ip"}"#);
    asker.send_raw(concat!(
        r#"{"op":"HttpLocal","phone":5,"request":12,"method":"POST","url":"http://192.168.1.1/cgi?1","#,
        r#""headers":{"Referer":"http://192.168.1.1/","Content-Type":"text/plain"},"body":"[x]","timeout_ms":5000}"#
    ));
    asker.send_raw(r#"{"op":"HttpLocal","phone":5,"request":13,"url":"http://192.168.1.1/"}"#);
    asker.send_raw(r#"{"op":"TypeText","phone":5,"request":14,"text":"Привет 👋 #тест\nстрока"}"#);
    asker.send_raw(r#"{"op":"ListApps","phone":5,"request":11,"id":2}"#);
    assert_eq!(asker.json()["ev"], "Ok");
    let t0 = Instant::now();
    while !phone.log().contains("ListApps") && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let log = phone.log();
    for want in [
        r#"LaunchApp { request: 7, package: "com.example.app" }"#,
        "UiDump { request: 8 }",
        r#"ExitInfo { request: 9, since_ms: 100, package: "com.example.app" }"#,
        r#"IpCheck { request: 10, url: "https://example.com/ip" }"#,
        concat!(
            r#"HttpLocal { request: 12, method: "POST", url: "http://192.168.1.1/cgi?1", headers: [("Content-Type", "text/plain"), "#,
            r#"("Referer", "http://192.168.1.1/")], body: "[x]", timeout_ms: 5000 }"#
        ),
        r#"HttpLocal { request: 13, method: "GET", url: "http://192.168.1.1/", headers: [], body: "", timeout_ms: 0 }"#,
        r#"TypeText { request: 14, text: "Привет 👋 #тест\nстрока" }"#,
        "ListApps { request: 11 }",
    ] {
        assert!(log.contains(want), "нет {want} в\n{log}");
    }

    // Ответ телефона приходит тому, кто спрашивал, хотя он не подписан на кадры; остальным — нет.
    phone.push(Event::Status(pcdock_core::packet::Status { code: 18, detail: "7 ok".into() }));
    match asker.msg() {
        ServerMsg::Status { phone: 5, code: 18, detail } => assert_eq!(detail, "7 ok"),
        m => panic!("ждали Status 18, пришло {m:?}"),
    }
    let reply =
        r#"12 {"ok":true,"code":401,"body":"[error]0","headers":{"set-cookie":"JSESSIONID=1"},"ms":30}"#;
    phone.push(Event::Status(pcdock_core::packet::Status { code: 24, detail: reply.into() }));
    match asker.msg() {
        ServerMsg::Status { phone: 5, code: 24, detail } => assert_eq!(detail, reply),
        m => panic!("ждали Status 24, пришло {m:?}"),
    }
    let reply = r#"14 {"ok":true,"method":"clipboard","reason":"no_service","ms":3}"#;
    phone.push(Event::Status(pcdock_core::packet::Status { code: 25, detail: reply.into() }));
    match asker.msg() {
        ServerMsg::Status { phone: 5, code: 25, detail } => assert_eq!(detail, reply),
        m => panic!("ждали Status 25, пришло {m:?}"),
    }
    other.send_raw(r#"{"op":"List","id":3}"#);
    let m = other.msg();
    assert!(matches!(m, ServerMsg::Phones { .. }), "Status не должен доходить до чужого клиента: {m:?}");
}
