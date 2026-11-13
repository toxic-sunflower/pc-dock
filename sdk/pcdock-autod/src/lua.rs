//! Движок сценариев: Lua 5.4 в песочнице, сценарий как набор тестов с проверками, логами и шагами.
//!
//! Песочница: доступны только `table`, `string`, `math`, `utf8`, `coroutine` и функции ниже; нет `io`, `os`,
//! `package`, `debug`, `load*`, `require`. Выполнение прерывается по запросу остановки и по сроку
//! (проверка идёт каждые 1000 инструкций и в каждом ожидании).
//!
//! Что видит сценарий:
//! - `test(name, fn)`, `before_each(fn)`, `after_each(fn)`: тесты; без `test` сам сценарий считается одним тестом;
//! - `assert(v, msg)`, `assert.eq/ne/truthy/falsy/near/contains/matches(...)`: проверка; провал прерывает тест;
//! - `expect.*`: те же проверки, но провал только отмечается, тест продолжается; `fail(msg)`: сразу провал;
//! - `step(text)`, `log.debug/info/warn/error(...)`, `sleep(sec)`, `now()`, `wait_until(fn, timeout, interval)`;
//! - `args`: параметры запуска; `phones()`, `phone(id)`: телефоны, выделенные прогону.
//!
//! Телефон (`phone`): `id`, `state()`, `info()`, `wait_ready(timeout)`, `tap(x, y, opts)`, `swipe(x1, y1, x2, y2, opts)`,
//! `touch(phase, x, y, opts)`, `key(name|usage, opts)`, `type(text)`, `scroll(ticks)`, `scroll_h(ticks)`, `wake()`,
//! `enable(kind, on)`, `frame(opts)`, `wait_frame(opts)`, `wait_change(timeout)`. Координаты: доли экрана 0..1.
//! Кадр (`frame`): `seq`, `pts`, `bytes`, `width`, `height`, `save(name)`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlua::{
    Function, HookTriggers, Lua, LuaOptions, MultiValue, RegistryKey, StdLib, Table, UserData,
    UserDataFields, UserDataMethods, Value, VmState,
};
use serde_json::{json, Value as Json};

use crate::backend::{Backend, Frame, PhoneInfo};
use crate::events::Sink;
use crate::jpeg::jpeg_size;
use crate::uitree::{self, Selector};

/// Параметры прогона.
pub struct RunConfig {
    /// Телефоны, выделенные прогону.
    pub phones: Vec<u32>,
    /// Параметры запуска (доступны сценарию как `args`).
    pub args: Json,
    /// Выполнить только эти тесты (по именам); `None` — все.
    pub only: Option<Vec<String>>,
    /// Срок на весь прогон.
    pub timeout: Option<Duration>,
    /// Куда складывать вложения (кадры); `None` — не сохранять.
    pub artifacts: Option<PathBuf>,
}

/// Итог одного теста.
#[derive(Clone, Debug, PartialEq)]
pub struct TestResult {
    pub name: String,
    /// `passed`, `failed` (проверка не прошла), `error` (ошибка в сценарии), `stopped`.
    pub status: String,
    pub duration_ms: u64,
    pub asserts_passed: u32,
    pub asserts_failed: u32,
    pub error: Option<String>,
}

/// Итог прогона.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// `passed`, `failed`, `error`, `stopped`.
    pub status: String,
    pub tests: Vec<TestResult>,
    pub duration_ms: u64,
}

impl Report {
    pub fn to_json(&self) -> Json {
        json!({
            "status": self.status,
            "duration_ms": self.duration_ms,
            "passed": self.tests.iter().filter(|t| t.status == "passed").count(),
            "failed": self.tests.iter().filter(|t| t.status == "failed").count(),
            "errors": self.tests.iter().filter(|t| t.status == "error").count(),
            "tests": self.tests.iter().map(|t| json!({
                "name": t.name, "status": t.status, "duration_ms": t.duration_ms,
                "asserts_passed": t.asserts_passed, "asserts_failed": t.asserts_failed, "error": t.error,
            })).collect::<Vec<_>>(),
        })
    }
}

struct State {
    sink: Arc<dyn Sink>,
    backend: Arc<dyn Backend>,
    stop: Arc<AtomicBool>,
    phones: Vec<u32>,
    artifacts: Option<PathBuf>,
    started: Instant,
    deadline: Option<Instant>,
    current: Mutex<String>,
    passed: Mutex<u32>,
    failed: Mutex<u32>,
    /// Провал `assert`/`fail` в текущем тесте: тест считается проваленным, а не сломанным.
    fatal: AtomicBool,
    failure_shots: Mutex<u32>,
    acquired: Mutex<Vec<u32>>,
}

impl State {
    fn emit(&self, kind: &str, fields: Json) {
        self.sink.emit(kind, fields);
    }

    fn check_alive(&self) -> mlua::Result<()> {
        if self.stop.load(Ordering::SeqCst) {
            return Err(mlua::Error::runtime("прогон остановлен"));
        }
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(mlua::Error::runtime("вышло время прогона"));
        }
        Ok(())
    }

    fn claimed(&self, id: u32) -> mlua::Result<()> {
        if self.phones.contains(&id) {
            Ok(())
        } else {
            Err(mlua::Error::runtime(format!("телефон {id} не выделен для этого прогона")))
        }
    }

    /// Ожидание с проверкой остановки каждые 20 мс.
    fn sleep(&self, d: Duration) -> mlua::Result<()> {
        let end = Instant::now() + d;
        loop {
            self.check_alive()?;
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(Duration::from_millis(20)));
        }
    }

    /// Результат проверки: событие `assert`, счётчики и, для `assert`, прерывание теста.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        mode: &str,
        op: &str,
        ok: bool,
        message: &str,
        expected: Option<String>,
        actual: Option<String>,
    ) -> mlua::Result<()> {
        if ok {
            *self.passed.lock().unwrap() += 1;
        } else {
            *self.failed.lock().unwrap() += 1;
        }
        self.emit(
            "assert",
            json!({"test": *self.current.lock().unwrap(), "mode": mode, "op": op, "ok": ok, "message": message,
                   "expected": expected, "actual": actual}),
        );
        if !ok {
            self.shoot_failure();
            if mode == "assert" {
                self.fatal.store(true, Ordering::SeqCst);
                return Err(mlua::Error::runtime(format!("проверка не прошла: {message}")));
            }
        }
        Ok(())
    }

    /// При провале сохраняет последние кадры телефонов прогона (не больше трёх раз на прогон).
    fn shoot_failure(&self) {
        let Some(dir) = &self.artifacts else { return };
        {
            let mut n = self.failure_shots.lock().unwrap();
            if *n >= 3 {
                return;
            }
            *n += 1;
        }
        let test = self.current.lock().unwrap().clone();
        for id in &self.phones {
            if let Some(f) = self.backend.latest_frame(*id) {
                let name = format!("fail_{}_{}_{}.jpg", sanitize(&test), id, f.seq);
                if std::fs::write(dir.join(&name), f.jpeg.as_slice()).is_ok() {
                    self.emit(
                        "artifact",
                        json!({"name": name, "kind": "frame", "phone": id, "seq": f.seq, "reason": "assert"}),
                    );
                }
            }
        }
    }

    fn acquire(&self, id: u32) -> mlua::Result<()> {
        let mut held = self.acquired.lock().unwrap();
        if !held.contains(&id) {
            self.backend.acquire(id).map_err(mlua::Error::runtime)?;
            held.push(id);
        }
        Ok(())
    }
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).take(40).collect()
}

// ---------------------------------------------------------------------------------------------
// Значения

fn show(v: &Value) -> String {
    match v {
        Value::Nil => "nil".into(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("{:?}", s.to_string_lossy()),
        Value::Table(t) => {
            let mut parts = Vec::new();
            for (n, pair) in t.clone().pairs::<Value, Value>().flatten().enumerate() {
                if n >= 8 {
                    parts.push("…".to_string());
                    break;
                }
                parts.push(format!("{} = {}", show(&pair.0), show(&pair.1)));
            }
            format!("{{{}}}", parts.join(", "))
        }
        Value::UserData(_) => "<userdata>".into(),
        Value::Function(_) => "<function>".into(),
        _ => "<?>".into(),
    }
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Number(n) => Some(*n),
        _ => None,
    }
}

fn equal(a: &Value, b: &Value, depth: u32) -> bool {
    if let (Some(x), Some(y)) = (num(a), num(b)) {
        return x == y;
    }
    match (a, b) {
        (Value::Nil, Value::Nil) => true,
        (Value::Boolean(x), Value::Boolean(y)) => x == y,
        (Value::String(x), Value::String(y)) => x.as_bytes() == y.as_bytes(),
        (Value::Table(x), Value::Table(y)) if depth < 8 => {
            let (mut n, mut m) = (0, 0);
            for (k, v) in x.clone().pairs::<Value, Value>().flatten() {
                n += 1;
                match y.get::<Value>(k) {
                    Ok(w) if equal(&v, &w, depth + 1) => {}
                    _ => return false,
                }
            }
            for _ in y.clone().pairs::<Value, Value>().flatten() {
                m += 1;
            }
            n == m
        }
        _ => false,
    }
}

fn truthy(v: &Value) -> bool {
    !matches!(v, Value::Nil | Value::Boolean(false))
}

fn opt_f32(t: &Option<Table>, key: &str) -> Option<f32> {
    t.as_ref().and_then(|t| t.get::<Option<f64>>(key).ok().flatten()).map(|x| x as f32)
}

fn opt_secs(t: &Option<Table>, key: &str, default: f64) -> Duration {
    Duration::from_secs_f64(
        t.as_ref().and_then(|t| t.get::<Option<f64>>(key).ok().flatten()).unwrap_or(default).max(0.0),
    )
}

// ---------------------------------------------------------------------------------------------
// Клавиши

/// Код USB HID по имени клавиши или числу.
fn key_usage(v: &Value) -> Option<u16> {
    if let Some(n) = num(v) {
        return u16::try_from(n as i64).ok();
    }
    let Value::String(s) = v else { return None };
    usage_for(&s.to_string_lossy())
}

fn usage_for(name: &str) -> Option<u16> {
    let name = name.to_lowercase();
    let n = name.as_str();
    if n.len() == 1 {
        let c = n.as_bytes()[0];
        return match c {
            b'a'..=b'z' => Some(0x04 + u16::from(c - b'a')),
            b'1'..=b'9' => Some(0x1E + u16::from(c - b'1')),
            b'0' => Some(0x27),
            b' ' => Some(0x2C),
            b'-' => Some(0x2D),
            b'=' => Some(0x2E),
            b'.' => Some(0x37),
            b',' => Some(0x36),
            b'/' => Some(0x38),
            _ => None,
        };
    }
    Some(match n {
        "enter" | "return" => 0x28,
        "esc" | "escape" | "back" => 0x29,
        "backspace" => 0x2A,
        "tab" => 0x2B,
        "space" => 0x2C,
        "right" => 0x4F,
        "left" => 0x50,
        "down" => 0x51,
        "up" => 0x52,
        "home" => 0x4A,
        "end" => 0x4D,
        "pageup" => 0x4B,
        "pagedown" => 0x4E,
        "delete" => 0x4C,
        "ctrl" | "lctrl" => 0xE0,
        "shift" | "lshift" => 0xE1,
        "alt" | "lalt" => 0xE2,
        "gui" | "meta" | "win" => 0xE3,
        _ if n.starts_with('f') && n.len() <= 3 => {
            let k: u16 = n[1..].parse().ok()?;
            if (1..=12).contains(&k) {
                0x3A + k - 1
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Телефон и кадр

struct PhoneHandle {
    id: u32,
    st: Arc<State>,
}

struct FrameHandle {
    f: Frame,
    phone: u32,
    st: Arc<State>,
}

fn info_table(lua: &Lua, i: &PhoneInfo) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("id", i.id)?;
    t.set("state", i.state.clone())?;
    t.set("transport", i.transport.clone())?;
    t.set("address", i.address.clone())?;
    t.set("serial", i.serial.clone())?;
    t.set("name", i.name.clone())?;
    if let Some((w, h)) = i.screen {
        t.set("width", w)?;
        t.set("height", h)?;
    }
    t.set("rotation", i.rotation)?;
    Ok(t)
}

impl UserData for FrameHandle {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("seq", |_, t| Ok(t.f.seq));
        fields.add_field_method_get("pts", |_, t| Ok(t.f.pts_us as f64 / 1e6));
        fields.add_field_method_get("bytes", |_, t| Ok(t.f.jpeg.len()));
        fields.add_field_method_get("width", |_, t| Ok(jpeg_size(&t.f.jpeg).map(|s| s.0)));
        fields.add_field_method_get("height", |_, t| Ok(jpeg_size(&t.f.jpeg).map(|s| s.1)));
        fields.add_field_method_get("phone", |_, t| Ok(t.phone));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Сохраняет кадр во вложения прогона; возвращает имя файла.
        methods.add_method("save", |_, t, name: Option<String>| {
            let Some(dir) = &t.st.artifacts else {
                return Err(mlua::Error::runtime("вложения выключены"));
            };
            let base = sanitize(&name.unwrap_or_else(|| format!("frame_{}_{}", t.phone, t.f.seq)));
            let file = format!("{base}.jpg");
            std::fs::write(dir.join(&file), t.f.jpeg.as_slice())
                .map_err(|e| mlua::Error::runtime(format!("запись кадра: {e}")))?;
            t.st.emit(
                "artifact",
                json!({"name": file, "kind": "frame", "phone": t.phone, "seq": t.f.seq, "reason": "script"}),
            );
            Ok(file)
        });
    }
}

impl PhoneHandle {
    fn touch(&self, phase: u32, x: f32, y: f32, o: &Option<Table>) -> mlua::Result<()> {
        self.st.check_alive()?;
        let finger = o.as_ref().and_then(|t| t.get::<Option<u8>>("finger").ok().flatten()).unwrap_or(0);
        self.st
            .backend
            .touch(self.id, phase, x, y, opt_f32(o, "pressure"), opt_f32(o, "size"), finger)
            .map_err(mlua::Error::runtime)
    }
}

/// Селектор элемента из Lua: строка — текст целиком, таблица — `{text=, text_contains=, id=, desc=, class=, index=}`.
fn selector(v: &Value) -> mlua::Result<Selector> {
    let sel = match v {
        Value::String(s) => Selector { text: Some(s.to_string_lossy()), ..Default::default() },
        Value::Table(t) => Selector {
            text: t.get("text")?,
            text_contains: t.get("text_contains")?,
            id: t.get("id")?,
            desc: t.get("desc")?,
            class: t.get("class")?,
            index: t.get::<Option<usize>>("index")?.unwrap_or(0),
        },
        _ => {
            return Err(mlua::Error::runtime(
                "элемент: строка (текст) или таблица {text=, id=, desc=, class=}",
            ))
        }
    };
    if sel.is_empty() {
        return Err(mlua::Error::runtime("элемент: задайте text, text_contains, id, desc или class"));
    }
    Ok(sel)
}

/// Ответ приложения как значение Lua: JSON — таблица, иначе строка.
fn reply_value(lua: &Lua, s: &str) -> mlua::Result<Value> {
    match serde_json::from_str::<Json>(s.trim()) {
        Ok(j) if j.is_object() || j.is_array() => json_to_lua(lua, &j),
        _ => Ok(Value::String(lua.create_string(s.trim())?)),
    }
}

/// Ответ `HttpLocal` → таблица `{ok, code, body, headers, ms}` или `{ok = false, error}`: JSON телефона как есть,
/// а ответ без JSON (`"error unsupported"` от сессии, если приложение pc-dock старое) — `{ok = false, error = …}`.
fn http_reply(lua: &Lua, s: &str) -> mlua::Result<Value> {
    if let Ok(j) = serde_json::from_str::<Json>(s.trim()) {
        if j.is_object() {
            return json_to_lua(lua, &j);
        }
    }
    let why = s.trim();
    let why = why.strip_prefix("error ").unwrap_or(why);
    json_to_lua(lua, &json!({"ok": false, "error": why}))
}

/// Можно ли набрать текст HID-клавиатурой `p:type` по-старому: латиница, цифры, пробел и `-=.,/`.
fn hid_typeable(text: &str) -> bool {
    text.chars().all(|c| c.is_ascii() && usage_for(&c.to_ascii_lowercase().to_string()).is_some())
}

/// Ответ `TypeText` (возможность 13) → способ ввода (`paste`, `set_text`, `clipboard`) или ошибка. Старое приложение
/// pc-dock (сессия ответила `error unsupported`) — ошибка с кодом `app_outdated` в начале сообщения.
fn text_input_reply(r: &str) -> Result<String, String> {
    let r = r.trim();
    match serde_json::from_str::<Json>(r) {
        Ok(j) if j.is_object() => {
            if j["ok"] == true {
                return Ok(j["method"].as_str().unwrap_or("").to_string());
            }
            Err(format!("text_input_failed: телефон не ввёл текст ({})", j["error"].as_str().unwrap_or("?")))
        }
        _ => match r.strip_prefix("error ").unwrap_or(r) {
            "unsupported" => Err("app_outdated: приложение pc-dock на телефоне не умеет вводить такой текст \
                                  (кириллица, эмодзи, знаки; нужна возможность 13 TEXT_INPUT) — обновите приложение pc-dock"
                .into()),
            why => Err(format!("text_input_failed: {why}")),
        },
    }
}

impl PhoneHandle {
    /// Ввести текст через приложение pc-dock (`TypeText`, возможность 13): телефон вставляет его в поле с курсором
    /// сам или кладёт в буфер обмена — тогда жмём Ctrl+V HID-клавиатурой. Возвращает способ ввода.
    fn text_input(&self, text: &str) -> mlua::Result<String> {
        let r = self.app(json!({"op": "TypeText", "text": text}), Duration::from_secs(15))?;
        let method = text_input_reply(&r).map_err(mlua::Error::runtime)?;
        if method == "clipboard" {
            // Ctrl (0xE0) + V (0x19)
            let k = |usage: u16, down: bool| {
                self.st.backend.key(self.id, usage, down).map_err(mlua::Error::runtime)
            };
            k(0xE0, true)?;
            k(0x19, true)?;
            k(0x19, false)?;
            k(0xE0, false)?;
        }
        Ok(method)
    }

    /// Запрос к приложению на телефоне с ответом-строкой.
    fn app(&self, req: Json, timeout: Duration) -> mlua::Result<String> {
        self.st.check_alive()?;
        self.st.backend.app(self.id, req, timeout).map_err(mlua::Error::runtime)
    }

    /// Элементы экрана сейчас; ошибка, если телефон их не отдал.
    fn ui_nodes(&self) -> mlua::Result<(String, Vec<uitree::Node>)> {
        let r = self.app(json!({"op": "UiDump"}), Duration::from_secs(10))?;
        let xml = r.strip_prefix("xml").map(str::trim_start).ok_or_else(|| {
            mlua::Error::runtime(format!(
                "элементы экрана недоступны: {}",
                r.chars().take(120).collect::<String>()
            ))
        })?;
        Ok((xml.to_string(), uitree::parse(xml)))
    }

    /// Ждёт элемент до срока (0 — одна попытка). Возвращает элемент и размер экрана.
    fn wait_node(
        &self,
        sel: &Selector,
        timeout: Duration,
    ) -> mlua::Result<Option<(uitree::Node, (i32, i32))>> {
        let end = Instant::now() + timeout;
        loop {
            // Окно ещё не готово (`no_window`: приложение только что открылось) — ждём до срока, как «не найден»
            let nodes = match self.ui_nodes() {
                Ok((_, nodes)) => nodes,
                Err(e) if e.to_string().contains("no_window") && Instant::now() < end => Vec::new(),
                Err(e) => return Err(e),
            };
            if let (Some(n), Some(size)) = (uitree::find(&nodes, sel), uitree::screen_size(&nodes)) {
                return Ok(Some((n.clone(), size)));
            }
            if Instant::now() >= end {
                return Ok(None);
            }
            self.st.sleep(Duration::from_millis(500))?;
        }
    }
}

fn node_table(lua: &Lua, n: &uitree::Node, size: (i32, i32)) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let (cx, cy) = n.center();
    t.set("text", n.text.as_str())?;
    t.set("id", n.id.as_str())?;
    t.set("desc", n.desc.as_str())?;
    t.set("class", n.class.as_str())?;
    t.set("package", n.package.as_str())?;
    t.set("clickable", n.clickable)?;
    t.set("enabled", n.enabled)?;
    // Центр в долях экрана — те же координаты, что у `p:tap`.
    t.set("x", cx / size.0 as f32)?;
    t.set("y", cy / size.1 as f32)?;
    let b = lua.create_table()?;
    for (i, v) in [n.bounds.0, n.bounds.1, n.bounds.2, n.bounds.3].into_iter().enumerate() {
        b.set(i + 1, v)?;
    }
    t.set("bounds", b)?;
    Ok(t)
}

impl UserData for PhoneHandle {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("id", |_, t| Ok(t.id));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("state", |_, t, ()| {
            Ok(t.st.backend.phone(t.id).map(|p| p.state).unwrap_or_else(|| "lost".into()))
        });
        methods.add_method("info", |lua, t, ()| match t.st.backend.phone(t.id) {
            Some(i) => Ok(Some(info_table(lua, &i)?)),
            None => Ok(None),
        });
        // Ждёт состояния `ready`; возвращает true или false по сроку.
        methods.add_method("wait_ready", |_, t, timeout: Option<f64>| {
            let end = Instant::now() + Duration::from_secs_f64(timeout.unwrap_or(30.0));
            loop {
                t.st.check_alive()?;
                if t.st.backend.phone(t.id).is_some_and(|p| p.state == "ready") {
                    return Ok(true);
                }
                if Instant::now() >= end {
                    return Ok(false);
                }
                t.st.sleep(Duration::from_millis(100))?;
            }
        });
        methods.add_method("tap", |_, t, (x, y, o): (f32, f32, Option<Table>)| {
            t.touch(0, x, y, &o)?;
            t.st.sleep(opt_secs(&o, "hold", 0.05))?;
            t.touch(2, x, y, &o)
        });
        methods.add_method("touch", |_, t, (phase, x, y, o): (Value, f32, f32, Option<Table>)| {
            let phase = match &phase {
                Value::String(s) => match s.to_string_lossy().as_str() {
                    "down" => 0,
                    "move" => 1,
                    "up" => 2,
                    "cancel" => 3,
                    other => {
                        return Err(mlua::Error::runtime(format!(
                            "неизвестная фаза касания {other:?} (down, move, up, cancel)"
                        )))
                    }
                },
                other => num(other)
                    .map(|n| n as u32)
                    .ok_or_else(|| mlua::Error::runtime("фаза касания: строка или число"))?,
            };
            t.touch(phase, x, y, &o)
        });
        methods.add_method("swipe", |_, t, (x1, y1, x2, y2, o): (f32, f32, f32, f32, Option<Table>)| {
            let total = opt_secs(&o, "duration", 0.3);
            let steps =
                o.as_ref().and_then(|t| t.get::<Option<u32>>("steps").ok().flatten()).unwrap_or(10).max(1);
            t.touch(0, x1, y1, &o)?;
            for i in 1..=steps {
                t.st.sleep(total / steps)?;
                let k = i as f32 / steps as f32;
                t.touch(1, x1 + (x2 - x1) * k, y1 + (y2 - y1) * k, &o)?;
            }
            t.touch(2, x2, y2, &o)
        });
        methods.add_method("key", |_, t, (k, o): (Value, Option<Table>)| {
            t.st.check_alive()?;
            let usage = key_usage(&k)
                .ok_or_else(|| mlua::Error::runtime(format!("неизвестная клавиша {}", show(&k))))?;
            t.st.backend.key(t.id, usage, true).map_err(mlua::Error::runtime)?;
            t.st.sleep(opt_secs(&o, "hold", 0.03))?;
            t.st.backend.key(t.id, usage, false).map_err(mlua::Error::runtime)
        });
        // Набор текста. Латиница, цифры, пробел и `-=.,/` — HID-клавиатурой (заглавные через Shift), как раньше;
        // остальное (кириллица, эмодзи, `#@!`, переводы строк) — через приложение pc-dock (возможность 13 TEXT_INPUT).
        // Старое приложение без неё — ошибка «app_outdated: …».
        methods.add_method("type", |_, t, text: mlua::String| {
            let text = text.to_str()?.to_string();
            if text.is_empty() {
                return Ok(());
            }
            if !hid_typeable(&text) {
                t.st.check_alive()?;
                return t.text_input(&text).map(|_| ());
            }
            for c in text.chars() {
                t.st.check_alive()?;
                let usage = usage_for(&c.to_ascii_lowercase().to_string())
                    .ok_or_else(|| mlua::Error::runtime(format!("символ {c:?} набрать нельзя")))?;
                let shift = c.is_ascii_uppercase();
                if shift {
                    t.st.backend.key(t.id, 0xE1, true).map_err(mlua::Error::runtime)?;
                }
                t.st.backend.key(t.id, usage, true).map_err(mlua::Error::runtime)?;
                t.st.backend.key(t.id, usage, false).map_err(mlua::Error::runtime)?;
                if shift {
                    t.st.backend.key(t.id, 0xE1, false).map_err(mlua::Error::runtime)?;
                }
            }
            Ok(())
        });
        // Вставить текст (любой) в поле с курсором через приложение pc-dock (возможность 13 TEXT_INPUT), без набора по
        // клавише; возвращает способ: "paste", "set_text" или "clipboard" (буфер обмена + Ctrl+V).
        methods.add_method("paste", |_, t, text: mlua::String| {
            let text = text.to_str()?.to_string();
            t.st.check_alive()?;
            t.text_input(&text)
        });
        methods.add_method("scroll", |_, t, ticks: f32| {
            t.st.check_alive()?;
            t.st.backend.scroll(t.id, ticks, false).map_err(mlua::Error::runtime)
        });
        methods.add_method("scroll_h", |_, t, ticks: f32| {
            t.st.check_alive()?;
            t.st.backend.scroll(t.id, ticks, true).map_err(mlua::Error::runtime)
        });
        methods.add_method("wake", |_, t, ()| {
            t.st.check_alive()?;
            t.st.backend.wake(t.id).map_err(mlua::Error::runtime)
        });
        methods.add_method("enable", |_, t, (kind, on): (String, bool)| {
            t.st.check_alive()?;
            let k = match kind.as_str() {
                "touch" => 0,
                "mouse" => 1,
                "keyboard" => 2,
                other => {
                    return Err(mlua::Error::runtime(format!(
                        "неизвестное устройство {other:?} (touch, mouse, keyboard)"
                    )))
                }
            };
            t.st.backend.enable(t.id, k, on).map_err(mlua::Error::runtime)
        });
        // Файлы на телефоне (нужен `pcdockd --files`): запись строки байт, чтение, список, удаление.
        methods.add_method("push", |lua, t, (dest, data, o): (String, mlua::String, Option<Table>)| {
            t.st.check_alive()?;
            let media = o.as_ref().and_then(|t| t.get::<Option<bool>>("media").ok().flatten()).unwrap_or(false);
            t.st.backend.file_put(t.id, &dest, &data.as_bytes(), media).map_err(mlua::Error::runtime)?;
            t.st.emit("log", json!({"test": *t.st.current.lock().unwrap(), "level": "debug", "text": format!("файл записан: {dest} ({} байт)", data.as_bytes().len())}));
            let _ = lua;
            Ok(())
        });
        methods.add_method("pull", |lua, t, (src, max): (String, Option<usize>)| {
            t.st.check_alive()?;
            let data =
                t.st.backend
                    .file_get(t.id, &src, max.unwrap_or(16 * 1024 * 1024))
                    .map_err(mlua::Error::runtime)?;
            lua.create_string(&data)
        });
        methods.add_method("ls", |lua, t, path: String| {
            t.st.check_alive()?;
            let names = t.st.backend.file_list(t.id, &path).map_err(mlua::Error::runtime)?;
            let out = lua.create_table()?;
            for (i, n) in names.iter().enumerate() {
                out.set(i + 1, n.as_str())?;
            }
            Ok(out)
        });
        methods.add_method("rm", |_, t, path: String| {
            t.st.check_alive()?;
            t.st.backend.file_delete(t.id, &path).map_err(mlua::Error::runtime)
        });
        // Приложение на телефоне (нужно приложение pc-dock с возможностями 6–12).
        // Открыть приложение по пакету; возвращает итог телефона: "ok", "not_installed", "blocked_bal", "error …".
        methods.add_method("launch", |_, t, (pkg, o): (String, Option<Table>)| {
            let r = t.app(json!({"op": "LaunchApp", "package": pkg}), opt_secs(&o, "timeout", 10.0))?;
            t.st.emit("log", json!({"test": *t.st.current.lock().unwrap(), "level": "debug", "text": format!("открыть {pkg}: {r}")}));
            Ok(r)
        });
        // Приложения с главным экраном: таблица из ответа телефона.
        methods.add_method("apps", |lua, t, ()| {
            let r = t.app(json!({"op": "ListApps"}), Duration::from_secs(15))?;
            reply_value(lua, &r)
        });
        // Пакет на переднем плане или nil.
        methods.add_method("foreground", |_, t, ()| Ok(t.st.backend.foreground(t.id)));
        // Элементы экрана как XML uiautomator.
        methods.add_method("ui", |_, t, ()| Ok(t.ui_nodes()?.0));
        // Найти элемент: `p:find("Войти")`, `p:find({id = "login"}, {timeout = 5})` → таблица {text, id, x, y, …} или nil.
        methods.add_method("find", |lua, t, (sel, o): (Value, Option<Table>)| {
            let sel = selector(&sel)?;
            match t.wait_node(&sel, opt_secs(&o, "timeout", 0.0))? {
                Some((n, size)) => Ok(Some(node_table(lua, &n, size)?)),
                None => Ok(None),
            }
        });
        // Ждёт появления элемента; true/false по сроку (по умолчанию 10 с).
        methods.add_method("wait_for", |_, t, (sel, timeout): (Value, Option<f64>)| {
            let sel = selector(&sel)?;
            Ok(t.wait_node(&sel, Duration::from_secs_f64(timeout.unwrap_or(10.0)))?.is_some())
        });
        // Нажать на элемент (ждёт его до `timeout`, по умолчанию 10 с); нет элемента — ошибка.
        methods.add_method("tap_on", |_, t, (sel, o): (Value, Option<Table>)| {
            let sel = selector(&sel)?;
            let Some((n, size)) = t.wait_node(&sel, opt_secs(&o, "timeout", 10.0))? else {
                return Err(mlua::Error::runtime(format!("элемент не найден: {}", sel.describe())));
            };
            let (cx, cy) = n.center();
            let (x, y) = (cx / size.0 as f32, cy / size.1 as f32);
            t.touch(0, x, y, &o)?;
            t.st.sleep(opt_secs(&o, "hold", 0.05))?;
            t.touch(2, x, y, &o)
        });
        // Причины завершения приложения после `since_ms` (мс Unix): таблица или строка ("unsupported"…).
        methods.add_method("exit_info", |lua, t, (pkg, since): (String, Option<u64>)| {
            let r = t.app(
                json!({"op": "ExitInfo", "package": pkg, "since_ms": since.unwrap_or(0)}),
                Duration::from_secs(10),
            )?;
            reply_value(lua, &r)
        });
        // Внешний IP телефона: HTTPS GET по его сети; таблица {body, code, network, …} или строка-ошибка.
        methods.add_method("ip", |lua, t, url: String| {
            let r = t.app(json!({"op": "IpCheck", "url": url}), Duration::from_secs(30))?;
            reply_value(lua, &r)
        });
        // HTTP-запрос к устройству локальной Wi-Fi-сети телефона (админка роутера и т.п.), через Wi-Fi телефона:
        // `p:http{method = "POST", url = "http://192.168.1.1/…", headers = {…}, body = "…", timeout = 10}` →
        // `{ok, code, body, headers, ms}` (любой код HTTP — ok = true) или `{ok = false, error}`.
        methods.add_method("http", |lua, t, o: Table| {
            let method =
                o.get::<Option<String>>("method")?.unwrap_or_else(|| "GET".into()).to_ascii_uppercase();
            let url: String =
                o.get::<Option<String>>("url")?.ok_or_else(|| mlua::Error::runtime("p:http: нужен url"))?;
            let mut headers = serde_json::Map::new();
            if let Some(h) = o.get::<Option<Table>>("headers")? {
                for pair in h.pairs::<String, String>() {
                    let (k, v) = pair?;
                    headers.insert(k, Json::String(v));
                }
            }
            let body = o
                .get::<Option<mlua::String>>("body")?
                .map(|b| String::from_utf8_lossy(&b.as_bytes()).into_owned())
                .unwrap_or_default();
            let timeout = o.get::<Option<f64>>("timeout")?.unwrap_or(10.0).clamp(0.001, 30.0);
            let timeout_ms = (timeout * 1000.0).round() as u32;
            let r = t.app(
                json!({"op": "HttpLocal", "method": method, "url": url, "headers": headers, "body": body,
                       "timeout_ms": timeout_ms}),
                Duration::from_millis(timeout_ms as u64) * 2 + Duration::from_secs(5),
            )?;
            http_reply(lua, &r)
        });
        // Последний кадр экрана; `{fresh = true}` — дождаться нового, `{timeout = N}` — срок ожидания.
        methods.add_method("frame", |_, t, o: Option<Table>| {
            t.st.acquire(t.id)?;
            let timeout = opt_secs(&o, "timeout", 5.0);
            let fresh =
                o.as_ref().and_then(|t| t.get::<Option<bool>>("fresh").ok().flatten()).unwrap_or(false);
            let after = if fresh { t.st.backend.latest_frame(t.id).map(|f| f.seq) } else { None };
            let end = Instant::now() + timeout;
            loop {
                t.st.check_alive()?;
                let slice = end.saturating_duration_since(Instant::now()).min(Duration::from_millis(200));
                if let Some(f) = t.st.backend.wait_frame(t.id, after, slice) {
                    return Ok(Some(FrameHandle { f, phone: t.id, st: t.st.clone() }));
                }
                if Instant::now() >= end {
                    return Ok(None);
                }
            }
        });
        // Ждёт, пока картинка не изменится относительно текущей; возвращает новый кадр или nil по сроку.
        methods.add_method("wait_change", |_, t, timeout: Option<f64>| {
            t.st.acquire(t.id)?;
            let end = Instant::now() + Duration::from_secs_f64(timeout.unwrap_or(5.0));
            let Some(first) = t.st.backend.wait_frame(t.id, None, Duration::from_secs(5)) else {
                return Ok(None);
            };
            let (mut seq, hash) = (first.seq, first.hash);
            loop {
                t.st.check_alive()?;
                let slice = end.saturating_duration_since(Instant::now()).min(Duration::from_millis(200));
                if let Some(f) = t.st.backend.wait_frame(t.id, Some(seq), slice) {
                    seq = f.seq;
                    if f.hash != hash {
                        return Ok(Some(FrameHandle { f, phone: t.id, st: t.st.clone() }));
                    }
                }
                if Instant::now() >= end {
                    return Ok(None);
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Запуск

struct Hooks {
    tests: Vec<(String, RegistryKey)>,
    before: Vec<RegistryKey>,
    after: Vec<RegistryKey>,
}

fn args_to_string(lua: &Lua, args: MultiValue) -> mlua::Result<String> {
    let tostring: Function = lua.globals().get("tostring")?;
    let mut parts = Vec::new();
    for a in args {
        parts.push(tostring.call::<String>(a)?);
    }
    Ok(parts.join(" "))
}

fn json_to_lua(lua: &Lua, v: &Json) -> mlua::Result<Value> {
    Ok(match v {
        Json::Null => Value::Nil,
        Json::Bool(b) => Value::Boolean(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Integer(i),
            None => Value::Number(n.as_f64().unwrap_or(0.0)),
        },
        Json::String(s) => Value::String(lua.create_string(s)?),
        Json::Array(a) => {
            let t = lua.create_table()?;
            for (i, x) in a.iter().enumerate() {
                t.set(i + 1, json_to_lua(lua, x)?)?;
            }
            Value::Table(t)
        }
        Json::Object(o) => {
            let t = lua.create_table()?;
            for (k, x) in o {
                t.set(k.as_str(), json_to_lua(lua, x)?)?;
            }
            Value::Table(t)
        }
    })
}

/// Таблица проверок `assert` или `expect`.
fn checks(lua: &Lua, st: &Arc<State>, mode: &'static str) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let msg = |m: Option<String>, d: String| m.unwrap_or(d);

    let s = st.clone();
    t.set(
        "eq",
        lua.create_function(move |_, (a, b, m): (Value, Value, Option<String>)| {
            let ok = equal(&a, &b, 0);
            s.record(
                mode,
                "eq",
                ok,
                &msg(m, format!("{} == {}", show(&a), show(&b))),
                Some(show(&b)),
                Some(show(&a)),
            )
        })?,
    )?;
    let s = st.clone();
    t.set(
        "ne",
        lua.create_function(move |_, (a, b, m): (Value, Value, Option<String>)| {
            let ok = !equal(&a, &b, 0);
            s.record(
                mode,
                "ne",
                ok,
                &msg(m, format!("{} != {}", show(&a), show(&b))),
                Some(format!("не {}", show(&b))),
                Some(show(&a)),
            )
        })?,
    )?;
    let s = st.clone();
    t.set(
        "truthy",
        lua.create_function(move |_, (v, m): (Value, Option<String>)| {
            s.record(
                mode,
                "truthy",
                truthy(&v),
                &msg(m, format!("{} истинно", show(&v))),
                Some("истина".into()),
                Some(show(&v)),
            )
        })?,
    )?;
    let s = st.clone();
    t.set(
        "falsy",
        lua.create_function(move |_, (v, m): (Value, Option<String>)| {
            s.record(
                mode,
                "falsy",
                !truthy(&v),
                &msg(m, format!("{} ложно", show(&v))),
                Some("ложь".into()),
                Some(show(&v)),
            )
        })?,
    )?;
    let s = st.clone();
    t.set(
        "near",
        lua.create_function(move |_, (a, b, eps, m): (f64, f64, Option<f64>, Option<String>)| {
            let eps = eps.unwrap_or(1e-6);
            s.record(
                mode,
                "near",
                (a - b).abs() <= eps,
                &msg(m, format!("{a} ≈ {b} (±{eps})")),
                Some(format!("{b} ± {eps}")),
                Some(a.to_string()),
            )
        })?,
    )?;
    let s = st.clone();
    t.set(
        "contains",
        lua.create_function(move |_, (text, sub, m): (String, String, Option<String>)| {
            s.record(
                mode,
                "contains",
                text.contains(&sub),
                &msg(m, format!("{text:?} содержит {sub:?}")),
                Some(format!("содержит {sub:?}")),
                Some(format!("{text:?}")),
            )
        })?,
    )?;
    // Проверка по шаблону Lua (`string.find`).
    let s = st.clone();
    t.set(
        "matches",
        lua.create_function(move |lua, (text, pattern, m): (String, String, Option<String>)| {
            let find: Function = lua.globals().get::<Table>("string")?.get("find")?;
            let found: Value = find.call((text.clone(), pattern.clone()))?;
            s.record(
                mode,
                "matches",
                truthy(&found),
                &msg(m, format!("{text:?} подходит под {pattern:?}")),
                Some(format!("шаблон {pattern:?}")),
                Some(format!("{text:?}")),
            )
        })?,
    )?;
    Ok(t)
}

/// Выполняет сценарий. Блокирует поток до конца прогона.
pub fn run_script(
    source: &str,
    script_name: &str,
    cfg: &RunConfig,
    backend: Arc<dyn Backend>,
    sink: Arc<dyn Sink>,
    stop: Arc<AtomicBool>,
) -> Report {
    let started = Instant::now();
    let st = Arc::new(State {
        sink: sink.clone(),
        backend,
        stop: stop.clone(),
        phones: cfg.phones.clone(),
        artifacts: cfg.artifacts.clone(),
        started,
        deadline: cfg.timeout.map(|d| started + d),
        current: Mutex::new(script_name.to_string()),
        passed: Mutex::new(0),
        failed: Mutex::new(0),
        fatal: AtomicBool::new(false),
        failure_shots: Mutex::new(0),
        acquired: Mutex::new(Vec::new()),
    });
    sink.emit("run_start", json!({"script": script_name, "phones": cfg.phones, "args": cfg.args}));

    let mut results: Vec<TestResult> = Vec::new();
    let outcome = execute(source, script_name, cfg, &st, &mut results);
    // Телефоны отпускаются: подписка на кадры снимается, видео на телефоне выключается, если смотреть больше некому.
    for id in st.acquired.lock().unwrap().drain(..) {
        st.backend.release(id);
    }
    let was_stopped = st.stop.load(Ordering::SeqCst);
    let status = match outcome {
        Err(e) if was_stopped || e.contains("остановлен") => "stopped",
        Err(e) if e.contains("вышло время") => "error",
        Err(_) => "error",
        Ok(()) if results.iter().any(|t| t.status == "error") => "error",
        Ok(()) if results.iter().any(|t| t.status == "failed") => "failed",
        Ok(()) if results.iter().any(|t| t.status == "stopped") => "stopped",
        Ok(()) => "passed",
    };
    let report = Report {
        status: status.to_string(),
        tests: results,
        duration_ms: started.elapsed().as_millis() as u64,
    };
    sink.emit("run_end", report.to_json());
    report
}

fn execute(
    source: &str,
    script_name: &str,
    cfg: &RunConfig,
    st: &Arc<State>,
    results: &mut Vec<TestResult>,
) -> Result<(), String> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE,
        LuaOptions::new(),
    )
    .map_err(|e| e.to_string())?;
    {
        let stop = st.stop.clone();
        let deadline = st.deadline;
        lua.set_hook(HookTriggers::new().every_nth_instruction(1000), move |_lua, _dbg| {
            if stop.load(Ordering::SeqCst) {
                return Err(mlua::Error::runtime("прогон остановлен"));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(mlua::Error::runtime("вышло время прогона"));
            }
            Ok(VmState::Continue)
        });
    }
    let hooks = Rc::new(RefCell::new(Hooks { tests: Vec::new(), before: Vec::new(), after: Vec::new() }));
    install(&lua, st, cfg, &hooks).map_err(|e| e.to_string())?;

    // Верхний уровень сценария. Пока нет зарегистрированных тестов, его проверки относятся к тесту «имя сценария».
    *st.current.lock().unwrap() = script_name.to_string();
    let t0 = Instant::now();
    let top = lua.load(source).set_name(format!("={script_name}")).exec();
    let registered = !hooks.borrow().tests.is_empty();
    if !registered {
        // Сценарий без `test`: сам сценарий — один тест.
        let (name, r) = finish_test(st, script_name, t0, top.map_err(|e| e.to_string()));
        st.sink.emit("test_start", json!({"test": name}));
        emit_end(st, &r);
        results.push(r);
        return Ok(());
    }
    if let Err(e) = top {
        return Err(format!("ошибка в сценарии: {e}"));
    }

    let tests: Vec<String> = hooks.borrow().tests.iter().map(|(n, _)| n.clone()).collect();
    for (index, name) in tests.iter().enumerate() {
        if cfg.only.as_ref().is_some_and(|only| !only.contains(name)) {
            continue;
        }
        if st.stop.load(Ordering::SeqCst) {
            return Err("прогон остановлен".into());
        }
        *st.current.lock().unwrap() = name.clone();
        *st.passed.lock().unwrap() = 0;
        *st.failed.lock().unwrap() = 0;
        st.fatal.store(false, Ordering::SeqCst);
        st.sink.emit("test_start", json!({"test": name}));
        let t0 = Instant::now();
        let outcome = run_test(&lua, &hooks, index);
        let (_, r) = finish_test(st, name, t0, outcome.map_err(|e| e.to_string()));
        emit_end(st, &r);
        let halted = r.status == "stopped";
        results.push(r);
        if halted {
            return Err("прогон остановлен".into());
        }
    }
    Ok(())
}

fn run_test(lua: &Lua, hooks: &Rc<RefCell<Hooks>>, index: usize) -> mlua::Result<()> {
    let (befores, test, afters) = {
        let h = hooks.borrow();
        let get = |k: &RegistryKey| lua.registry_value::<Function>(k);
        (
            h.before.iter().map(get).collect::<mlua::Result<Vec<_>>>()?,
            get(&h.tests[index].1)?,
            h.after.iter().map(get).collect::<mlua::Result<Vec<_>>>()?,
        )
    };
    let mut result = Ok(());
    for b in &befores {
        result = b.call::<()>(());
        if result.is_err() {
            break;
        }
    }
    if result.is_ok() {
        result = test.call::<()>(());
    }
    // `after_each` выполняется всегда; ошибка теста важнее ошибки уборки.
    for a in &afters {
        let r = a.call::<()>(());
        if result.is_ok() {
            result = r;
        }
    }
    result
}

fn finish_test(
    st: &Arc<State>,
    name: &str,
    t0: Instant,
    outcome: Result<(), String>,
) -> (String, TestResult) {
    let passed = *st.passed.lock().unwrap();
    let failed = *st.failed.lock().unwrap();
    let (status, error) = match outcome {
        Ok(()) if failed > 0 => ("failed", None),
        Ok(()) => ("passed", None),
        Err(e) if st.fatal.load(Ordering::SeqCst) => ("failed", Some(e)),
        Err(e) if e.contains("прогон остановлен") || e.contains("вышло время") => {
            (if e.contains("остановлен") { "stopped" } else { "error" }, Some(e))
        }
        Err(e) => ("error", Some(e)),
    };
    (
        name.to_string(),
        TestResult {
            name: name.to_string(),
            status: status.to_string(),
            duration_ms: t0.elapsed().as_millis() as u64,
            asserts_passed: passed,
            asserts_failed: failed,
            error,
        },
    )
}

fn emit_end(st: &Arc<State>, r: &TestResult) {
    st.sink.emit(
        "test_end",
        json!({"test": r.name, "status": r.status, "duration_ms": r.duration_ms, "asserts_passed": r.asserts_passed,
               "asserts_failed": r.asserts_failed, "error": r.error}),
    );
}

/// Регистрирует в Lua всё, что видит сценарий.
fn install(lua: &Lua, st: &Arc<State>, cfg: &RunConfig, hooks: &Rc<RefCell<Hooks>>) -> mlua::Result<()> {
    let g = lua.globals();
    // Из стандартных библиотек убираем то, что вернёт загрузку кода.
    for name in ["dofile", "loadfile", "load", "require", "collectgarbage"] {
        g.set(name, Value::Nil)?;
    }
    g.set("args", json_to_lua(lua, &cfg.args)?)?;

    // Тесты.
    let h = hooks.clone();
    g.set(
        "test",
        lua.create_function(move |lua, (name, f): (String, Function)| {
            h.borrow_mut().tests.push((name, lua.create_registry_value(f)?));
            Ok(())
        })?,
    )?;
    let h = hooks.clone();
    g.set(
        "before_each",
        lua.create_function(move |lua, f: Function| {
            h.borrow_mut().before.push(lua.create_registry_value(f)?);
            Ok(())
        })?,
    )?;
    let h = hooks.clone();
    g.set(
        "after_each",
        lua.create_function(move |lua, f: Function| {
            h.borrow_mut().after.push(lua.create_registry_value(f)?);
            Ok(())
        })?,
    )?;

    // Проверки: `assert` вызывается как функция и как таблица.
    let assert_t = checks(lua, st, "assert")?;
    let meta = lua.create_table()?;
    let s = st.clone();
    meta.set(
        "__call",
        lua.create_function(move |_, (_this, v, m): (Value, Value, Option<String>)| {
            s.record(
                "assert",
                "truthy",
                truthy(&v),
                &m.unwrap_or_else(|| "assert".into()),
                Some("истина".into()),
                Some(show(&v)),
            )
        })?,
    )?;
    assert_t.set_metatable(Some(meta));
    g.set("assert", assert_t)?;
    g.set("expect", checks(lua, st, "expect")?)?;
    let s = st.clone();
    g.set(
        "fail",
        lua.create_function(move |_, m: Option<String>| {
            s.record("assert", "fail", false, &m.unwrap_or_else(|| "fail".into()), None, None)
        })?,
    )?;

    // Шаги, логи, время.
    let s = st.clone();
    g.set(
        "step",
        lua.create_function(move |lua, args: MultiValue| {
            s.emit("step", json!({"test": *s.current.lock().unwrap(), "text": args_to_string(lua, args)?}));
            Ok(())
        })?,
    )?;
    let log = lua.create_table()?;
    for level in ["debug", "info", "warn", "error"] {
        let s = st.clone();
        log.set(
            level,
            lua.create_function(move |lua, args: MultiValue| {
                s.emit("log", json!({"test": *s.current.lock().unwrap(), "level": level, "text": args_to_string(lua, args)?}));
                Ok(())
            })?,
        )?;
    }
    g.set("log", log)?;
    let s = st.clone();
    g.set(
        "sleep",
        lua.create_function(move |_, secs: f64| s.sleep(Duration::from_secs_f64(secs.max(0.0))))?,
    )?;
    let s = st.clone();
    g.set("now", lua.create_function(move |_, ()| Ok(s.started.elapsed().as_secs_f64()))?)?;
    let s = st.clone();
    g.set(
        "wait_until",
        lua.create_function(move |_, (f, timeout, interval): (Function, Option<f64>, Option<f64>)| {
            let end = Instant::now() + Duration::from_secs_f64(timeout.unwrap_or(5.0));
            let every = Duration::from_secs_f64(interval.unwrap_or(0.1).max(0.01));
            loop {
                s.check_alive()?;
                let v: Value = f.call(())?;
                if truthy(&v) {
                    return Ok(true);
                }
                if Instant::now() >= end {
                    return Ok(false);
                }
                s.sleep(every)?;
            }
        })?,
    )?;

    // Телефоны.
    let s = st.clone();
    g.set(
        "phones",
        lua.create_function(move |lua, ()| {
            let t = lua.create_table()?;
            for (i, id) in s.phones.iter().enumerate() {
                t.set(i + 1, PhoneHandle { id: *id, st: s.clone() })?;
            }
            Ok(t)
        })?,
    )?;
    let s = st.clone();
    g.set(
        "phone",
        lua.create_function(move |_, id: u32| {
            s.claimed(id)?;
            Ok(PhoneHandle { id, st: s.clone() })
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{frame_hash, Phase};
    use crate::events::Collect;
    use std::sync::atomic::AtomicU64;

    /// Фейковая ферма: один телефон, журнал действий, кадры по запросу.
    #[derive(Default)]
    struct Fake {
        log: Mutex<Vec<String>>,
        frames: Mutex<Vec<Frame>>,
        seq: AtomicU64,
        state: Mutex<String>,
        files: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            let f = Arc::new(Self::default());
            *f.state.lock().unwrap() = "ready".into();
            f
        }

        fn push_frame(&self, data: Vec<u8>) {
            let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
            let hash = frame_hash(&data);
            self.frames.lock().unwrap().push(Frame { seq, pts_us: seq * 1000, jpeg: Arc::new(data), hash });
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Backend for Fake {
        fn phones(&self) -> Vec<PhoneInfo> {
            vec![PhoneInfo {
                id: 1,
                state: self.state.lock().unwrap().clone(),
                serial: Some("SER1".into()),
                screen: Some((720, 1640)),
                ..Default::default()
            }]
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
            self.log
                .lock()
                .unwrap()
                .push(format!("touch {phone} {phase} {x:.2} {y:.2} p={pressure:?} s={size:?} f={finger}"));
            Ok(())
        }
        fn key(&self, phone: u32, usage: u16, down: bool) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("key {phone} {usage:#x} {down}"));
            Ok(())
        }
        fn scroll(&self, phone: u32, ticks: f32, horizontal: bool) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("scroll {phone} {ticks} {horizontal}"));
            Ok(())
        }
        fn wake(&self, phone: u32) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("wake {phone}"));
            Ok(())
        }
        fn enable(&self, phone: u32, kind: u32, on: bool) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("enable {phone} {kind} {on}"));
            Ok(())
        }
        fn acquire(&self, phone: u32) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("acquire {phone}"));
            Ok(())
        }
        fn release(&self, phone: u32) {
            self.log.lock().unwrap().push(format!("release {phone}"));
        }
        fn file_put(&self, _phone: u32, dest: &str, data: &[u8], _media: bool) -> Result<(), String> {
            self.files.lock().unwrap().insert(dest.to_string(), data.to_vec());
            Ok(())
        }
        fn file_get(&self, _phone: u32, src: &str, _max: usize) -> Result<Vec<u8>, String> {
            self.files.lock().unwrap().get(src).cloned().ok_or_else(|| format!("нет файла {src}"))
        }
        fn file_list(&self, _phone: u32, path: &str) -> Result<Vec<String>, String> {
            let prefix = format!("{path}/");
            Ok(self
                .files
                .lock()
                .unwrap()
                .keys()
                .filter_map(|k| k.strip_prefix(&prefix).map(String::from))
                .collect())
        }
        fn file_delete(&self, _phone: u32, path: &str) -> Result<(), String> {
            self.files.lock().unwrap().remove(path);
            Ok(())
        }
        fn app(&self, phone: u32, req: Json, _timeout: Duration) -> Result<String, String> {
            self.log.lock().unwrap().push(format!("app {phone} {req}"));
            Ok(match req["op"].as_str() {
                Some("LaunchApp") => "ok".into(),
                Some("ListApps") => r#"[{"package":"com.app","label":"App"}]"#.into(),
                Some("UiDump") => concat!(
                    r#"xml <hierarchy rotation="0"><node text="" class="android.widget.FrameLayout" bounds="[0,0][1000,2000]">"#,
                    r#"<node text="Войти" resource-id="com.app:id/login" class="android.widget.Button" clickable="true" bounds="[100,1000][300,1200]" />"#,
                    "</node></hierarchy>"
                )
                .into(),
                Some("IpCheck") => r#"{"code":200,"body":"1.2.3.4"}"#.into(),
                Some("HttpLocal") if req["url"] == "http://192.168.1.1/cgi?1" => {
                    r#"{"ok":true,"code":401,"body":"[error]0","headers":{"set-cookie":"JSESSIONID=1"},"ms":30}"#.into()
                }
                Some("HttpLocal") => "error unsupported".into(),
                Some("TypeText") => match req["text"].as_str().unwrap_or("") {
                    "старое приложение" => "error unsupported".into(),
                    t if t.starts_with("буфер") => r#"{"ok":true,"method":"clipboard","reason":"no_service","ms":2}"#.into(),
                    "сбой" => r#"{"ok":false,"error":"clipboard: SecurityException","ms":1}"#.into(),
                    _ => r#"{"ok":true,"method":"paste","ms":40}"#.into(),
                },
                _ => "unsupported".into(),
            })
        }
        fn foreground(&self, _phone: u32) -> Option<String> {
            Some("com.app".into())
        }
        fn latest_frame(&self, _phone: u32) -> Option<Frame> {
            self.frames.lock().unwrap().last().cloned()
        }
        fn wait_frame(&self, phone: u32, after: Option<u64>, timeout: Duration) -> Option<Frame> {
            let end = Instant::now() + timeout;
            loop {
                if let Some(f) = self.latest_frame(phone) {
                    if after.is_none_or(|a| f.seq > a) {
                        return Some(f);
                    }
                }
                if Instant::now() >= end {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn cfg() -> RunConfig {
        RunConfig {
            phones: vec![1],
            args: json!({"level": 3, "name": "x"}),
            only: None,
            timeout: Some(Duration::from_secs(20)),
            artifacts: None,
        }
    }

    fn run(src: &str, fake: &Arc<Fake>) -> (Report, Arc<Collect>) {
        run_with(src, fake, cfg())
    }

    fn run_with(src: &str, fake: &Arc<Fake>, cfg: RunConfig) -> (Report, Arc<Collect>) {
        let sink = Arc::new(Collect::default());
        let r = run_script(src, "demo", &cfg, fake.clone(), sink.clone(), Arc::new(AtomicBool::new(false)));
        (r, sink)
    }

    #[test]
    fn type_uses_text_input_for_unicode_and_hid_for_plain_latin() {
        let fake = Fake::new();
        let (r, _sink) = run(
            r#"
            test("text", function()
              local p = phones()[1]
              p:type("ab")
              p:type("")
              p:type("Привет 👋 #тест\nстрока")
              assert.eq(p:paste("plain"), "paste")
              assert.eq(p:paste("буфер ✨"), "clipboard")
              local ok, e = pcall(function() p:type("старое приложение") end)
              assert.falsy(ok)
              assert.contains(tostring(e), "app_outdated")
              local ok2, e2 = pcall(function() p:paste("сбой") end)
              assert.falsy(ok2)
              assert.contains(tostring(e2), "text_input_failed")
              assert.contains(tostring(e2), "SecurityException")
            end)
            "#,
            &fake,
        );
        assert_eq!(r.status, "passed", "{r:?}");
        let log = fake.log();
        let apps: Vec<&String> = log.iter().filter(|l| l.contains(r#""op":"TypeText""#)).collect();
        assert_eq!(apps.len(), 5, "{log:?}");
        assert!(apps[0].contains(r#""text":"Привет 👋 #тест\nстрока""#), "{apps:?}");
        assert!(!log.iter().any(|l| l.contains(r#""text":"ab""#)), "латиница — HID, не TEXT_INPUT");
        // «ab» — HID (a = 0x4, b = 0x5); после ответа «clipboard» — Ctrl+V: Ctrl (0xE0) вниз, V (0x19), Ctrl вверх
        let keys: Vec<&str> = log.iter().filter(|l| l.starts_with("key")).map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "key 1 0x4 true",
                "key 1 0x4 false",
                "key 1 0x5 true",
                "key 1 0x5 false",
                "key 1 0xe0 true",
                "key 1 0x19 true",
                "key 1 0x19 false",
                "key 1 0xe0 false",
            ]
        );
    }

    #[test]
    fn app_requests_and_screen_elements_from_lua() {
        let fake = Fake::new();
        let (r, _sink) = run(
            r#"
            test("app", function()
              local p = phones()[1]
              assert.eq(p:launch("com.app"), "ok")
              assert.eq(p:apps()[1].package, "com.app")
              assert.eq(p:foreground(), "com.app")
              assert.eq(p:ip("https://example.com/ip").body, "1.2.3.4")
              local h = p:http{method = "post", url = "http://192.168.1.1/cgi?1", headers = {Referer = "http://192.168.1.1/"},
                               body = "[x]", timeout = 5}
              assert.truthy(h.ok)
              assert.eq(h.code, 401)
              assert.eq(h.body, "[error]0")
              assert.eq(h.headers["set-cookie"], "JSESSIONID=1")
              local old = p:http{url = "http://192.168.1.1/"}
              assert.falsy(old.ok)
              assert.eq(old.error, "unsupported")
              assert.eq(p:exit_info("com.app", 5), "unsupported")
              local e = p:find({id = "login"})
              assert.eq(e.text, "Войти")
              assert.near(e.x, 0.2, 0.001)
              assert.near(e.y, 0.55, 0.001)
              assert.truthy(p:wait_for("войти", 1))
              assert.falsy(p:find("Нет такого"))
              p:tap_on("Войти")
              assert.contains(p:ui(), "hierarchy")
            end)
            "#,
            &fake,
        );
        assert_eq!(r.status, "passed", "{r:?}");
        let log = fake.log().join("\n");
        assert!(log.contains(r#""op":"LaunchApp""#) && log.contains(r#""package":"com.app""#), "{log}");
        assert!(
            log.contains(r#""method":"POST""#)
                && log.contains(r#""timeout_ms":5000"#)
                && log.contains(r#""headers":{"Referer":"http://192.168.1.1/"}"#),
            "{log}"
        );
        assert!(log.contains("touch 1 0 0.20 0.55") && log.contains("touch 1 2 0.20 0.55"), "{log}");
    }

    #[test]
    fn passing_and_failing_tests_are_reported_with_asserts() {
        let fake = Fake::new();
        let (r, sink) = run(
            r#"
            test("ok", function()
              assert.eq(1 + 1, 2)
              assert(true, "истина")
              expect.eq("a", "a")
            end)
            test("bad", function()
              assert.eq(1, 2, "один не два")
              log.info("до сюда не дойдёт")
            end)
            "#,
            &fake,
        );
        assert_eq!(r.status, "failed");
        assert_eq!(
            r.tests.iter().map(|t| (t.name.as_str(), t.status.as_str())).collect::<Vec<_>>(),
            vec![("ok", "passed"), ("bad", "failed")]
        );
        assert_eq!((r.tests[0].asserts_passed, r.tests[1].asserts_failed), (3, 1));
        let asserts = sink.of("assert");
        let bad = asserts.iter().find(|a| a["ok"] == false).unwrap();
        assert_eq!(
            (bad["op"].as_str(), bad["message"].as_str(), bad["expected"].as_str(), bad["actual"].as_str()),
            (Some("eq"), Some("один не два"), Some("2"), Some("1"))
        );
        assert!(sink.of("log").is_empty(), "после провала assert тест не продолжается");
        let kinds = sink.kinds();
        assert_eq!(kinds.first().map(String::as_str), Some("run_start"));
        assert_eq!(kinds.last().map(String::as_str), Some("run_end"));
    }

    #[test]
    fn expect_records_failure_and_continues() {
        let fake = Fake::new();
        let (r, sink) = run(
            r#"
            test("soft", function()
              expect.eq(1, 2)
              log.info("продолжаем")
              expect.truthy(true)
            end)
            "#,
            &fake,
        );
        assert_eq!(r.tests[0].status, "failed");
        assert_eq!((r.tests[0].asserts_passed, r.tests[0].asserts_failed), (1, 1));
        assert_eq!(sink.of("log")[0]["text"], "продолжаем");
    }

    #[test]
    fn script_without_tests_is_one_test_named_after_the_script() {
        let fake = Fake::new();
        let (r, _) = run("assert.eq(args.level, 3)\nassert.eq(args.name, 'x')", &fake);
        assert_eq!((r.status.as_str(), r.tests.len(), r.tests[0].name.as_str()), ("passed", 1, "demo"));
        let (r, _) = run("assert.eq(args.level, 4)", &fake);
        assert_eq!((r.status.as_str(), r.tests[0].status.as_str()), ("failed", "failed"));
    }

    #[test]
    fn runtime_errors_are_errors_not_failures() {
        let fake = Fake::new();
        let (r, _) = run("test('boom', function() local x = nil; return x.y end)", &fake);
        assert_eq!((r.status.as_str(), r.tests[0].status.as_str()), ("error", "error"));
        assert!(r.tests[0].error.as_ref().unwrap().contains("nil"), "{:?}", r.tests[0].error);
        let (r, _) = run("this is not lua", &fake);
        assert_eq!(r.status, "error", "синтаксическая ошибка сценария");
    }

    #[test]
    fn sandbox_has_no_io_os_or_loaders() {
        let fake = Fake::new();
        let (r, _) = run(
            "assert.eq(io, nil); assert.eq(os, nil); assert.eq(package, nil); assert.eq(debug, nil)\n\
             assert.eq(require, nil); assert.eq(load, nil); assert.eq(dofile, nil); assert.eq(loadfile, nil)\n\
             assert.truthy(string.format('%d', 3) == '3')",
            &fake,
        );
        assert_eq!(r.status, "passed", "{r:?}");
    }

    #[test]
    fn before_and_after_each_wrap_every_test_even_on_failure() {
        let fake = Fake::new();
        let (r, sink) = run(
            r#"
            before_each(function() step("before") end)
            after_each(function() step("after") end)
            test("a", function() assert.eq(1, 2) end)
            test("b", function() end)
            "#,
            &fake,
        );
        assert_eq!(r.status, "failed");
        let steps: Vec<String> =
            sink.of("step").iter().map(|s| s["text"].as_str().unwrap().to_string()).collect();
        assert_eq!(steps, vec!["before", "after", "before", "after"]);
    }

    #[test]
    fn only_filters_tests_by_name() {
        let fake = Fake::new();
        let mut c = cfg();
        c.only = Some(vec!["two".into()]);
        let (r, _) =
            run_with("test('one', function() fail('нет') end)\ntest('two', function() end)", &fake, c);
        assert_eq!((r.status.as_str(), r.tests.len()), ("passed", 1));
    }

    #[test]
    fn stop_flag_interrupts_even_a_busy_loop() {
        let fake = Fake::new();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            s.store(true, Ordering::SeqCst);
        });
        let sink = Arc::new(Collect::default());
        let t0 = Instant::now();
        let r =
            run_script("test('loop', function() while true do end end)", "demo", &cfg(), fake, sink, stop);
        t.join().unwrap();
        assert_eq!(r.status, "stopped");
        assert!(t0.elapsed() < Duration::from_secs(5), "бесконечный цикл остановлен");
    }

    #[test]
    fn run_timeout_ends_a_long_sleep() {
        let fake = Fake::new();
        let mut c = cfg();
        c.timeout = Some(Duration::from_millis(200));
        let t0 = Instant::now();
        let (r, _) = run_with("test('slow', function() sleep(30) end)", &fake, c);
        assert_eq!(r.status, "error");
        assert!(r.tests[0].error.as_ref().unwrap().contains("вышло время"), "{:?}", r.tests[0].error);
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn phone_commands_reach_the_backend() {
        let fake = Fake::new();
        let (r, _) = run(
            r#"
            local p = phones()[1]
            assert.eq(p.id, 1)
            assert.eq(p:state(), "ready")
            assert.eq(p:info().serial, "SER1")
            assert.eq(p:info().width, 720)
            p:tap(0.5, 0.25, {pressure = 0.7, size = 0.05, hold = 0})
            p:swipe(0.5, 0.9, 0.5, 0.1, {duration = 0, steps = 2})
            p:key("esc")
            p:type("Hi 1")
            p:scroll(-2)
            p:wake()
            p:enable("mouse", false)
            "#,
            &fake,
        );
        assert_eq!(r.status, "passed", "{r:?}");
        let log = fake.log();
        assert_eq!(log[0], "touch 1 0 0.50 0.25 p=Some(0.7) s=Some(0.05) f=0");
        assert_eq!(log[1], "touch 1 2 0.50 0.25 p=Some(0.7) s=Some(0.05) f=0");
        // Свайп: нажатие, два промежуточных шага, отпускание.
        assert_eq!(
            log[2..6].iter().map(|l| l.split(' ').nth(2).unwrap()).collect::<Vec<_>>(),
            vec!["0", "1", "1", "2"]
        );
        assert!(
            log.contains(&"key 1 0x29 true".to_string()) && log.contains(&"key 1 0x29 false".to_string()),
            "Esc"
        );
        // Заглавная H набирается через Shift (0xE1), потом h (0x0B).
        let typed: Vec<&String> =
            log.iter().filter(|l| l.starts_with("key") && l.ends_with("true")).collect();
        assert!(
            typed.iter().any(|l| l.contains("0xe1")) && typed.iter().any(|l| l.contains("0xb")),
            "{typed:?}"
        );
        assert!(log.contains(&"scroll 1 -2 false".to_string()));
        assert!(log.contains(&"wake 1".to_string()));
        assert!(log.contains(&"enable 1 1 false".to_string()));
    }

    #[test]
    fn unknown_phone_and_bad_arguments_are_clear_errors() {
        let fake = Fake::new();
        let (r, _) = run("phone(7)", &fake);
        assert!(r.tests[0].error.as_ref().unwrap().contains("не выделен"), "{r:?}");
        let (r, _) = run("phones()[1]:key('нет такой')", &fake);
        assert!(r.tests[0].error.as_ref().unwrap().contains("неизвестная клавиша"), "{r:?}");
        let (r, _) = run("phones()[1]:touch('sideways', 0, 0)", &fake);
        assert!(r.tests[0].error.as_ref().unwrap().contains("фаза касания"), "{r:?}");
    }

    #[test]
    fn frames_are_acquired_once_released_at_the_end_and_saved_as_artifacts() {
        let fake = Fake::new();
        // Минимальный JPEG с размером 560x1280 в заголовке.
        let jpeg = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x05, 0x00, 0x02,
            0x30, 0x01, 0x01, 0x11, 0x00,
        ];
        fake.push_frame(jpeg);
        let dir = std::env::temp_dir().join(format!("pcdock-lua-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = cfg();
        c.artifacts = Some(dir.clone());
        let (r, sink) = run_with(
            r#"
            local p = phones()[1]
            local f = p:frame()
            assert.eq(f.width, 560)
            assert.eq(f.height, 1280)
            assert.truthy(f.bytes > 10)
            local again = p:frame()
            assert.eq(again.seq, f.seq)
            f:save("home")
            "#,
            &fake,
            c,
        );
        assert_eq!(r.status, "passed", "{r:?}");
        let log = fake.log();
        assert_eq!(log.iter().filter(|l| l.starts_with("acquire")).count(), 1, "подписка берётся один раз");
        assert_eq!(log.last().map(String::as_str), Some("release 1"), "и снимается в конце прогона");
        assert!(dir.join("home.jpg").exists());
        assert_eq!(sink.of("artifact")[0]["name"], "home.jpg");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_assert_saves_the_latest_frame_as_artifact() {
        let fake = Fake::new();
        fake.push_frame(vec![1, 2, 3]);
        let dir = std::env::temp_dir().join(format!("pcdock-lua-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = cfg();
        c.artifacts = Some(dir.clone());
        let (r, sink) = run_with("test('t1', function() assert.eq(1, 2) end)", &fake, c);
        assert_eq!(r.status, "failed");
        let art = sink.of("artifact");
        assert_eq!(art.len(), 1);
        assert_eq!(art[0]["reason"], "assert");
        assert!(dir.join(art[0]["name"].as_str().unwrap()).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_change_returns_when_the_picture_changes() {
        let fake = Fake::new();
        fake.push_frame(vec![1]);
        let f2 = fake.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            f2.push_frame(vec![1]); // тот же кадр: изменения нет
            std::thread::sleep(Duration::from_millis(100));
            f2.push_frame(vec![2]); // другой
        });
        let (r, _) =
            run("local f = phones()[1]:wait_change(5)\nassert.truthy(f ~= nil)\nassert.eq(f.seq, 3)", &fake);
        t.join().unwrap();
        assert_eq!(r.status, "passed", "{r:?}");
    }

    #[test]
    fn wait_ready_and_wait_until() {
        let fake = Fake::new();
        *fake.state.lock().unwrap() = "connecting".into();
        let f2 = fake.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            *f2.state.lock().unwrap() = "ready".into();
        });
        let (r, _) = run(
            "assert.truthy(phones()[1]:wait_ready(5))\nlocal n = 0\nassert.truthy(wait_until(function() n = n + 1; return n >= 3 end, 2, 0.01))\nassert.falsy(wait_until(function() return false end, 0.1, 0.02))",
            &fake,
        );
        t.join().unwrap();
        assert_eq!(r.status, "passed", "{r:?}");
    }

    #[test]
    fn checks_compare_tables_numbers_and_strings() {
        let fake = Fake::new();
        let (r, _) = run(
            r#"
            assert.eq({1, 2, {a = 1}}, {1, 2, {a = 1}})
            assert.ne({1}, {2})
            assert.eq(1, 1.0)
            assert.near(0.1 + 0.2, 0.3, 1e-9)
            assert.contains("hello world", "lo wo")
            assert.matches("abc123", "%d+")
            assert.falsy(nil)
            "#,
            &fake,
        );
        assert_eq!(r.status, "passed", "{r:?}");
    }

    #[test]
    fn files_can_be_pushed_pulled_listed_and_removed() {
        let fake = Fake::new();
        let (r, _) = run(
            r#"
            local p = phones()[1]
            p:push("/sdcard/Download/a.txt", "привет\0мир", {media = true})
            assert.eq(p:pull("/sdcard/Download/a.txt"), "привет\0мир")
            assert.eq(p:ls("/sdcard/Download"), {"a.txt"})
            p:rm("/sdcard/Download/a.txt")
            assert.eq(#p:ls("/sdcard/Download"), 0)
            local ok, err = pcall(function() return p:pull("/sdcard/none") end)
            assert.falsy(ok)
            assert.contains(tostring(err), "нет файла")
            "#,
            &fake,
        );
        assert_eq!(r.status, "passed", "{r:?}");
    }

    #[test]
    fn report_json_has_counts_and_tests() {
        let fake = Fake::new();
        let (r, sink) = run("test('a', function() end)\ntest('b', function() assert.eq(1,2) end)", &fake);
        let j = r.to_json();
        assert_eq!(
            (j["passed"].as_u64(), j["failed"].as_u64(), j["errors"].as_u64()),
            (Some(1), Some(1), Some(0))
        );
        assert_eq!(j["tests"][1]["name"], "b");
        assert_eq!(sink.of("run_end")[0]["status"], "failed");
    }
}
