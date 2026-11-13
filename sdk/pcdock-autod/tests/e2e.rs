//! Сквозные тесты демона автоматизации: настоящий `pcdockd` (сервер фермы с фейковым телефоном), настоящий клиент
//! протокола, движок Lua, прогоны, прямой эфир событий, отчёт и вложения.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pcdock_autod::backend::FarmBackend;
use pcdock_autod::runs::Runs;
use pcdock_autod::service;
use pcdock_autod::store::ScriptStore;
use pcdock_client::Client;
use pcdock_core::link::{
    AppRequest, Discovery, InputCmd, LinkError, LinkFactory, PhoneLink, Sighting, Transport,
};
use pcdock_core::session::Event;
use pcdock_farm::{DiscoveryConfig, Farm};
use pcdock_serve::{FileBackend, FileOptions, FileWriter, Serve};
use serde_json::{json, Value};

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
        std::thread::sleep(Duration::from_millis(2));
        let mut q = self.events.lock().unwrap();
        let n = q.len().min(16);
        Ok(q.drain(..n).collect())
    }
    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError> {
        self.inputs.lock().unwrap().push(format!("{cmd:?}"));
        // Приложение на телефоне отвечает на запросы, как настоящее: `Status` с номером запроса.
        if let InputCmd::App(req) = cmd {
            let id = req.request();
            let replies: Vec<(u16, String)> = match req {
                AppRequest::LaunchApp { .. } => vec![(18, format!("{id} ok"))],
                AppRequest::HttpLocal { url, .. } if url == "http://192.168.1.1/" => {
                    vec![(
                        24,
                        format!(r#"{id} {{"ok":true,"code":401,"body":"[error]0","headers":{{}},"ms":7}}"#),
                    )]
                }
                // Поля с фокусом нет — текст только в буфере обмена, Ctrl+V жмёт хост
                AppRequest::TypeText { .. } => {
                    vec![(
                        25,
                        format!(r#"{id} {{"ok":true,"method":"clipboard","reason":"no_focus","ms":3}}"#),
                    )]
                }
                AppRequest::UiDump { .. } => vec![
                    (20, format!(r#"{id} 1/2 xml <hierarchy><node text="" bounds="[0,0][1000,2000]">"#)),
                    (
                        20,
                        format!(r#"{id} 2/2 <node text="OK" bounds="[0,0][200,200]" /></node></hierarchy>"#),
                    ),
                ],
                _ => vec![(req.reply_code(), format!("{id} unsupported"))],
            };
            let mut q = self.events.lock().unwrap();
            for (code, detail) in replies {
                q.push_back(Event::Status(pcdock_core::packet::Status { code, detail }));
            }
        }
        Ok(())
    }
    fn shutdown(&mut self) {}
}

/// Минимальный JPEG с размером 560x1280 в заголовке.
fn jpeg(extra: u8) -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x05, 0x00, 0x02, 0x30,
        0x01, 0x01, 0x11, extra,
    ]
}

struct Env {
    phone_events: Arc<Mutex<VecDeque<Event>>>,
    phone_inputs: Arc<Mutex<Vec<String>>>,
    autod: std::net::SocketAddr,
    runs: Arc<Runs>,
    _serve: Serve,
    _dir: std::path::PathBuf,
}

impl Env {
    fn client(&self) -> Client {
        Client::connect_tcp(&self.autod.to_string(), None).unwrap()
    }

    fn put(&self, c: &Client, name: &str, source: &str) -> String {
        c.request(json!({"op": "ScriptPut", "name": name, "source": source})).unwrap()["script"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn push_frame(&self, extra: u8) {
        self.phone_events.lock().unwrap().push_back(Event::Video(pcdock_core::packet::VideoFrame {
            codec: 1,
            key_frame: true,
            pts_us: u64::from(extra),
            data: jpeg(extra),
        }));
    }

    fn inputs(&self) -> String {
        self.phone_inputs.lock().unwrap().join("\n")
    }

    fn wait_inputs(&self, needle: &str) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            if self.inputs().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

fn env(name: &str) -> Env {
    // Ферма с одним фейковым телефоном и сервер `pcdockd`.
    let phone_events = Arc::new(Mutex::new(VecDeque::new()));
    let phone_inputs = Arc::new(Mutex::new(Vec::new()));
    let mut farm = Farm::new();
    farm.add_phone(
        1,
        Box::new(FakeLink { connected: false, events: phone_events.clone(), inputs: phone_inputs.clone() }),
    )
    .unwrap();
    phone_events.lock().unwrap().push_back(Event::Connected {
        version: 2,
        capabilities: 0,
        peer_name: "fake".into(),
    });
    let serve = Serve::new(farm);
    let farm_addr = serve.run_tcp("127.0.0.1:0").unwrap();

    // Демон автоматизации поверх клиента к `pcdockd`.
    let dir = std::env::temp_dir().join(format!("pcdock-autod-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let client = Client::connect_tcp(&farm_addr.to_string(), None).unwrap();
    let backend = FarmBackend::new(client, 5.0).unwrap();
    // Телефон должен успеть стать готовым.
    let t0 = Instant::now();
    while backend.refresh().is_ok()
        && !pcdock_autod::backend::Backend::phones(&*backend).iter().any(|p| p.state == "ready")
    {
        assert!(t0.elapsed() < Duration::from_secs(5), "телефон не стал ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    let store = Arc::new(ScriptStore::open(&dir).unwrap());
    let runs = Runs::new(&dir, store.clone(), backend.clone()).unwrap();
    let hub = service::build(store, runs.clone(), backend, None);
    let autod = hub.run_tcp("127.0.0.1:0").unwrap();
    Env { phone_events, phone_inputs, autod, runs, _serve: serve, _dir: dir }
}

/// Читает события до `RunEnded` этого прогона; возвращает события прогона и итог.
fn follow(events: &std::sync::mpsc::Receiver<Value>, run: &str) -> (Vec<Value>, Value) {
    let mut out = Vec::new();
    let t0 = Instant::now();
    loop {
        let v = events.recv_timeout(Duration::from_secs(10)).expect("событие прогона не пришло");
        assert!(t0.elapsed() < Duration::from_secs(30));
        if v["ev"] == "RunEvent" && v["run"] == run {
            let end = v["kind"] == "run_end";
            out.push(v.clone());
            if end {
                return (out, v);
            }
        }
    }
}

#[test]
fn script_runs_as_a_test_with_live_events_and_a_report() {
    let e = env("basic");
    let c = e.client();
    assert_eq!(c.hello()["role"], "autod");
    let events = c.take_events().unwrap();
    let id = e.put(
        &c,
        "Главный экран",
        r#"
        test("тап", function()
          step("нажимаем")
          local p = phones()[1]
          assert.truthy(p:wait_ready(5))
          p:tap(0.5, 0.5, {hold = 0, pressure = 0.7})
          log.info("готово", 42)
          assert.eq(args.name, "демо")
        end)
        test("провал", function()
          expect.eq(1, 2, "ожидаемо плохо")
        end)
        "#,
    );
    let started = c.request(json!({"op": "Run", "script": id, "args": {"name": "демо"}})).unwrap();
    assert_eq!(started["ev"], "RunStarted");
    let run = started["run"].as_str().unwrap().to_string();
    assert_eq!(started["phones"], json!([1]), "по умолчанию выделены все готовые телефоны");

    let (evs, end) = follow(&events, &run);
    let kinds: Vec<&str> = evs.iter().map(|v| v["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"run_start"));
    assert!(kinds.contains(&"step") && kinds.contains(&"log") && kinds.contains(&"assert"), "{kinds:?}");
    // Номера событий идут подряд с нуля.
    assert!(evs.iter().enumerate().all(|(i, v)| v["seq"] == i as u64), "seq подряд");
    assert_eq!(end["status"], "failed");
    assert_eq!((end["passed"].as_u64(), end["failed"].as_u64()), (Some(1), Some(1)));
    // Касание дошло до телефона через ферму.
    assert!(e.wait_inputs("Touch"), "{}", e.inputs());

    // Отчёт и список прогонов; всем клиентам пришло RunEnded.
    assert!(e.runs.wait(&run, Duration::from_secs(5)));
    let info = c.request(json!({"op": "RunInfo", "run": run})).unwrap();
    assert_eq!(info["report"]["status"], "failed");
    assert_eq!(info["report"]["report"]["tests"][0]["name"], "тап");
    let list = c.request(json!({"op": "Runs"})).unwrap();
    assert_eq!(list["runs"][0]["run"], run.as_str());
    assert_eq!(list["runs"][0]["status"], "failed");
}

#[test]
fn late_subscriber_gets_the_whole_history() {
    let e = env("history");
    let c = e.client();
    let id = e.put(&c, "s", "log.info('привет')");
    let run = c.request(json!({"op": "Run", "script": id, "follow": false})).unwrap()["run"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(e.runs.wait(&run, Duration::from_secs(5)));

    let late = e.client();
    let events = late.take_events().unwrap();
    late.request(json!({"op": "Subscribe", "run": run, "from": 0})).unwrap();
    let (evs, _) = follow(&events, &run);
    assert!(evs.iter().any(|v| v["kind"] == "log" && v["text"] == "привет"));
    // С нужного номера: хвост истории.
    let tail = e.client();
    let tail_events = tail.take_events().unwrap();
    tail.request(json!({"op": "Subscribe", "run": run, "from": evs.len() as u64 - 1})).unwrap();
    let (only_end, _) = follow(&tail_events, &run);
    assert_eq!(only_end.len(), 1, "только последнее событие");
}

#[test]
fn frames_are_requested_from_the_farm_only_while_a_script_needs_them() {
    let e = env("frames");
    let c = e.client();
    e.push_frame(1);
    let id = e.put(
        &c,
        "frames",
        r#"
        local p = phones()[1]
        local f = p:frame({timeout = 5})
        assert.truthy(f)
        assert.eq(f.width, 560)
        assert.eq(f.height, 1280)
        f:save("первый")
        "#,
    );
    // Кадры идут только после подписки: ждём её и кладём кадр.
    let run = c.request(json!({"op": "Run", "script": id})).unwrap()["run"].as_str().unwrap().to_string();
    assert!(e.wait_inputs("video: true, audio: false, max_fps: 5"), "{}", e.inputs());
    e.push_frame(2);
    assert!(e.runs.wait(&run, Duration::from_secs(10)));
    let info = c.request(json!({"op": "RunInfo", "run": run})).unwrap();
    assert_eq!(info["report"]["status"], "passed", "{info}");
    // После прогона подписка снята: видео на телефоне выключено.
    assert!(e.wait_inputs("video: false"), "{}", e.inputs());
    assert!(
        info["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "first.jpg" || a.as_str().unwrap().ends_with(".jpg")),
        "{info}"
    );
}

#[test]
fn phones_are_exclusive_and_a_run_can_be_stopped() {
    let e = env("busy");
    let c = e.client();
    let slow = e.put(&c, "долгий", "test('x', function() sleep(30) end)");
    let quick = e.put(&c, "быстрый", "log.info('x')");
    let run = c.request(json!({"op": "Run", "script": slow, "phones": [1]})).unwrap()["run"]
        .as_str()
        .unwrap()
        .to_string();
    let err = c.request(json!({"op": "Run", "script": quick, "phones": [1]})).unwrap_err();
    assert!(err.to_string().contains("занят"), "{err}");
    let err = c.request(json!({"op": "Run", "script": quick, "phones": [9]})).unwrap_err();
    assert!(err.to_string().contains("нет телефона"), "{err}");

    c.request(json!({"op": "Stop", "run": run})).unwrap();
    assert!(e.runs.wait(&run, Duration::from_secs(5)));
    let info = c.request(json!({"op": "RunInfo", "run": run})).unwrap();
    assert_eq!(info["report"]["status"], "stopped");
    // Телефон освободился.
    c.request(json!({"op": "Run", "script": quick, "phones": [1], "follow": false})).unwrap();
}

#[test]
fn failure_screenshot_is_listed_and_downloadable_over_the_wire() {
    let e = env("shots");
    let c = e.client();
    e.push_frame(7);
    let id = e.put(
        &c,
        "shot",
        "local p = phones()[1]\nassert.truthy(p:frame({timeout = 5}))\nassert.eq(1, 2, 'экран не тот')",
    );
    // Подписка (и кадры) появляется после старта: докладываем кадр, когда телефон о ней узнает.
    let run = c.request(json!({"op": "Run", "script": id})).unwrap()["run"].as_str().unwrap().to_string();
    assert!(e.wait_inputs("video: true"), "{}", e.inputs());
    e.push_frame(8);
    assert!(e.runs.wait(&run, Duration::from_secs(10)));

    let names = c.request(json!({"op": "Artifacts", "run": run})).unwrap();
    let name = names["names"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n.as_str())
        .find(|n| n.starts_with("fail_"))
        .expect("снимок провала")
        .to_string();
    let bin = c.take_binary().unwrap();
    let meta = c.request(json!({"op": "Artifact", "run": run, "name": name})).unwrap();
    assert_eq!(meta["ev"], "Artifact");
    let (kind, payload) = bin.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(kind, service::KIND_ARTIFACT);
    let name_len = payload[0] as usize;
    assert_eq!(std::str::from_utf8(&payload[1..1 + name_len]).unwrap(), name);
    assert_eq!(&payload[1 + name_len..1 + name_len + 2], &[0xFF, 0xD8], "это JPEG");

    // Путь за пределы папки прогона не отдаётся.
    for bad in ["../report.json", "report.json", "events.jsonl", ".hidden", "a/b"] {
        assert!(c.request(json!({"op": "Artifact", "run": run, "name": bad})).is_err(), "{bad}");
    }
}

#[test]
fn script_crud_over_the_wire_keeps_names_next_to_files() {
    let e = env("crud");
    let c = e.client();
    let id = e.put(&c, "Меню", "log.info('a')");
    let list = c.request(json!({"op": "ScriptList"})).unwrap();
    assert_eq!(list["scripts"][0]["name"], "Меню");
    assert_eq!(list["scripts"][0]["script"], id.as_str());
    assert_eq!(c.request(json!({"op": "ScriptGet", "script": id})).unwrap()["source"], "log.info('a')");
    c.request(json!({"op": "ScriptPut", "script": id, "name": "Меню 2", "source": "log.info('b')"})).unwrap();
    c.request(json!({"op": "ScriptRename", "script": id, "name": "Главное меню"})).unwrap();
    assert_eq!(c.request(json!({"op": "ScriptGet", "script": id})).unwrap()["name"], "Главное меню");
    assert!(c.request(json!({"op": "ScriptGet", "script": "../etc/passwd"})).is_err());
    c.request(json!({"op": "ScriptDelete", "script": id})).unwrap();
    assert!(c.request(json!({"op": "ScriptGet", "script": id})).is_err());
    assert!(c.request(json!({"op": "Nope"})).is_err());
}

#[test]
fn phones_list_comes_from_the_farm() {
    let e = env("phones");
    let c = e.client();
    let r = c.request(json!({"op": "Phones"})).unwrap();
    assert_eq!(r["phones"], json!([1]));
    assert_eq!(r["details"][0]["state"], "ready");
}

// ---- файлы: сценарий Lua → автоматизация → pcdockd → «телефон» ----------------------------------

#[derive(Default)]
struct MemFs(Mutex<std::collections::BTreeMap<String, Vec<u8>>>);

struct MemWriter(Arc<MemFs>, String, Vec<u8>);

impl FileWriter for MemWriter {
    fn write(&mut self, data: &[u8]) -> Result<(), String> {
        self.2.extend_from_slice(data);
        Ok(())
    }
    fn finish(self: Box<Self>, size: u64, _media: bool) -> Result<(), String> {
        if self.2.len() as u64 != size {
            return Err("размер не сошёлся".into());
        }
        self.0 .0.lock().unwrap().insert(self.1, self.2);
        Ok(())
    }
}

struct Files(Arc<MemFs>);

impl FileBackend for Files {
    fn put(&self, _s: &str, dest: &str) -> Result<Box<dyn FileWriter>, String> {
        Ok(Box::new(MemWriter(self.0.clone(), dest.to_string(), Vec::new())))
    }
    fn get(&self, _s: &str, src: &str) -> Result<(u64, Box<dyn std::io::Read + Send>), String> {
        let d = self.0 .0.lock().unwrap().get(src).cloned().ok_or_else(|| format!("нет файла {src}"))?;
        Ok((d.len() as u64, Box::new(std::io::Cursor::new(d))))
    }
    fn list(&self, _s: &str, path: &str) -> Result<Vec<String>, String> {
        let prefix = format!("{path}/");
        Ok(self
            .0
             .0
            .lock()
            .unwrap()
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix).map(String::from))
            .collect())
    }
    fn delete(&self, _s: &str, path: &str) -> Result<(), String> {
        self.0 .0.lock().unwrap().remove(path);
        Ok(())
    }
}

struct Scan;
impl Discovery for Scan {
    fn scan(&mut self) -> Vec<Sighting> {
        vec![Sighting {
            transport: Transport::Adb,
            address: "A".into(),
            serial: Some("SER1".into()),
            issue: None,
        }]
    }
}
struct Factory(Arc<Mutex<VecDeque<Event>>>);
impl LinkFactory for Factory {
    fn make(&mut self, _: &Sighting) -> Result<Box<dyn PhoneLink>, String> {
        Ok(Box::new(FakeLink {
            connected: false,
            events: self.0.clone(),
            inputs: Arc::new(Mutex::new(Vec::new())),
        }))
    }
}

#[test]
fn lua_scripts_move_files_through_the_whole_chain() {
    let fs = Arc::new(MemFs::default());
    let events = Arc::new(Mutex::new(VecDeque::new()));
    events.lock().unwrap().push_back(Event::Connected {
        version: 2,
        capabilities: 0,
        peer_name: "fake".into(),
    });
    let mut farm = Farm::new();
    farm.enable_discovery(
        vec![Box::new(Scan)],
        Box::new(Factory(events)),
        DiscoveryConfig { every: Duration::ZERO, ..Default::default() },
    );
    farm.discover_now();
    let opts = FileOptions { backend: Arc::new(Files(fs.clone())), roots: vec!["/sdcard".into()] };
    let serve = Serve::with_options(farm, None, Some(opts));
    let farm_addr = serve.run_tcp("127.0.0.1:0").unwrap();

    let dir = std::env::temp_dir().join(format!("pcdock-autod-e2e-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let backend = FarmBackend::new(Client::connect_tcp(&farm_addr.to_string(), None).unwrap(), 5.0).unwrap();
    let t0 = Instant::now();
    while backend.refresh().is_ok()
        && !pcdock_autod::backend::Backend::phones(&*backend).iter().any(|p| p.state == "ready")
    {
        assert!(t0.elapsed() < Duration::from_secs(5), "телефон не стал ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    let store = Arc::new(ScriptStore::open(&dir).unwrap());
    let runs = Runs::new(&dir, store.clone(), backend.clone()).unwrap();
    let hub = service::build(store, runs.clone(), backend, None);
    let addr = hub.run_tcp("127.0.0.1:0").unwrap();
    let c = Client::connect_tcp(&addr.to_string(), None).unwrap();

    let big = "x".repeat(200_000);
    let id = c
        .request(json!({"op": "ScriptPut", "name": "files", "source": r#"
        local p = phones()[1]
        p:push("/sdcard/Download/note.txt", "привет")
        p:push("/sdcard/Download/big.txt", string.rep("x", 200000), {media = true})
        assert.eq(p:pull("/sdcard/Download/note.txt"), "привет")
        assert.eq(#p:pull("/sdcard/Download/big.txt"), 200000)
        local names = p:ls("/sdcard/Download")
        table.sort(names)
        assert.eq(names, {"big.txt", "note.txt"})
        p:rm("/sdcard/Download/note.txt")
        assert.eq(p:ls("/sdcard/Download"), {"big.txt"})
        local ok = pcall(function() p:push("/system/x", "no") end)
        assert.falsy(ok)
        "#}))
        .unwrap()["script"]
        .as_str()
        .unwrap()
        .to_string();
    let run = c.request(json!({"op": "Run", "script": id, "follow": false})).unwrap()["run"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(runs.wait(&run, Duration::from_secs(30)));
    let info = c.request(json!({"op": "RunInfo", "run": run})).unwrap();
    assert_eq!(info["report"]["status"], "passed", "{info}");
    assert_eq!(fs.0.lock().unwrap().get("/sdcard/Download/big.txt").map(Vec::len), Some(big.len()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lua_asks_the_phone_app_through_the_farm_daemon() {
    let e = env("app");
    let c = e.client();
    let events = c.take_events().unwrap();
    let id = e.put(
        &c,
        "Приложение",
        r#"
        test("app", function()
          local p = phones()[1]
          assert.eq(p:launch("com.example.app"), "ok")
          local ok = p:find("OK")
          assert.near(ok.x, 0.1, 0.001)
          p:tap_on("OK")
          assert.eq(p:ip("https://example.com/ip"), "unsupported")
          local r = p:http{method = "POST", url = "http://192.168.1.1/", body = "[x]"}
          assert.truthy(r.ok)
          assert.eq(r.code, 401)
          assert.eq(r.body, "[error]0")
          p:type("Подпись ✨ #тест")
          assert.eq(p:paste("x"), "clipboard")
        end)
        "#,
    );
    let started = c.request(json!({"op": "Run", "script": id})).unwrap();
    let run = started["run"].as_str().unwrap().to_string();
    let (evs, end) = follow(&events, &run);
    assert_eq!(end["status"], "passed", "{evs:?}");
    assert!(e.wait_inputs(r#"LaunchApp { request: "#), "{}", e.inputs());
    assert!(e.inputs().contains("UiDump"), "{}", e.inputs());
    assert!(e.inputs().contains(r#"HttpLocal { request: "#), "{}", e.inputs());
    assert!(e.inputs().contains(r#"text: "Подпись ✨ #тест" }"#), "{}", e.inputs());
    assert!(e.inputs().contains("Key { usage: 224, down: true }"), "Ctrl+V после clipboard: {}", e.inputs());
    assert!(e.inputs().contains("Key { usage: 25, down: true }"), "Ctrl+V после clipboard: {}", e.inputs());
}
