//! Прогоны: запуск сценария в своём потоке, захват телефонов, остановка, отчёт на диске.
//!
//! Папка прогона: `runs/<id>/` с `events.jsonl` (все события), `report.json` (итог) и вложениями (кадры).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::backend::Backend;
use crate::events::{RunLog, Sink};
use crate::lua::{run_script_ex, RunConfig};
use crate::store::ScriptStore;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Один прогон.
pub struct Run {
    pub id: String,
    pub script_id: String,
    pub script_name: String,
    pub phones: Vec<u32>,
    pub started_ms: u64,
    pub dir: PathBuf,
    pub log: Arc<RunLog>,
    stop: Arc<AtomicBool>,
    abort: Arc<Mutex<Option<String>>>,
    finished: Mutex<Option<(u64, String)>>,
}

impl Run {
    /// Краткие сведения для списка.
    pub fn summary(&self) -> Value {
        let fin = self.finished.lock().unwrap().clone();
        json!({
            "run": self.id, "script": self.script_id, "name": self.script_name, "phones": self.phones,
            "started": self.started_ms,
            "finished": fin.as_ref().map(|f| f.0),
            "status": fin.map(|f| f.1).unwrap_or_else(|| "running".into()),
        })
    }

    pub fn is_finished(&self) -> bool {
        self.finished.lock().unwrap().is_some()
    }
}

type Notify = Box<dyn Fn(&Value) + Send + Sync>;

pub struct Runs {
    data: PathBuf,
    store: Arc<ScriptStore>,
    backend: Arc<dyn Backend>,
    runs: Mutex<HashMap<String, Arc<Run>>>,
    /// Телефон → прогон, который им владеет.
    busy: Mutex<HashMap<u32, String>>,
    notify: Mutex<Option<Notify>>,
}

/// Допустимое имя вложения: без путей.
pub fn valid_artifact(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 120
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

impl Runs {
    pub fn new(data: &Path, store: Arc<ScriptStore>, backend: Arc<dyn Backend>) -> Result<Arc<Self>, String> {
        let dir = data.join("runs");
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Arc::new(Self {
            data: data.to_path_buf(),
            store,
            backend,
            runs: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashMap::new()),
            notify: Mutex::new(None),
        }))
    }

    /// Обработчик уведомлений `RunStarted` / `RunEnded` (для рассылки клиентам).
    pub fn set_notify(&self, f: Notify) {
        *self.notify.lock().unwrap() = Some(f);
    }

    fn notify(&self, v: Value) {
        if let Some(f) = self.notify.lock().unwrap().as_ref() {
            f(&v);
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Run>> {
        self.runs.lock().unwrap().get(id).cloned()
    }

    /// Прогоны текущего запуска демона и сохранённые на диске, новые первыми.
    pub fn list(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self.runs.lock().unwrap().values().map(|r| r.summary()).collect();
        let known: Vec<String> = out.iter().filter_map(|v| v["run"].as_str().map(String::from)).collect();
        if let Ok(rd) = fs::read_dir(self.data.join("runs")) {
            for e in rd.flatten() {
                let id = e.file_name().to_string_lossy().to_string();
                if known.contains(&id) {
                    continue;
                }
                if let Ok(text) = fs::read_to_string(e.path().join("report.json")) {
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        out.push(json!({"run": id, "script": v["script"]["id"], "name": v["script"]["name"], "phones": v["phones"],
                                        "started": v["started"], "finished": v["finished"], "status": v["status"]}));
                    }
                }
            }
        }
        out.sort_by_key(|v| std::cmp::Reverse(v["started"].as_u64().unwrap_or(0)));
        out
    }

    /// Итоговый отчёт прогона с диска.
    pub fn report(&self, id: &str) -> Option<Value> {
        if !valid_artifact(id) {
            return None;
        }
        serde_json::from_str(&fs::read_to_string(self.data.join("runs").join(id).join("report.json")).ok()?)
            .ok()
    }

    /// Файлы-вложения прогона.
    pub fn artifacts(&self, id: &str) -> Vec<String> {
        if !valid_artifact(id) {
            return Vec::new();
        }
        let mut v: Vec<String> = fs::read_dir(self.data.join("runs").join(id))
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
            .unwrap_or_default();
        v.retain(|n| n != "events.jsonl" && n != "report.json");
        v.sort();
        v
    }

    pub fn artifact(&self, id: &str, name: &str) -> Result<Vec<u8>, String> {
        if !valid_artifact(id) || !valid_artifact(name) || name == "events.jsonl" || name == "report.json" {
            return Err("плохое имя вложения".into());
        }
        fs::read(self.data.join("runs").join(id).join(name)).map_err(|e| format!("вложение {name}: {e}"))
    }

    /// Запускает сценарий. `phones` — выделяемые телефоны (по умолчанию все готовые).
    pub fn start(
        self: &Arc<Self>,
        script_id: &str,
        phones: Option<Vec<u32>>,
        args: Value,
        only: Option<Vec<String>>,
        timeout: Option<Duration>,
    ) -> Result<Arc<Run>, String> {
        let (meta, source) = self.store.get(script_id)?;
        let known = self.backend.phones();
        let wanted: Vec<u32> = match phones {
            Some(p) => {
                for id in &p {
                    if !known.iter().any(|k| k.id == *id) {
                        return Err(format!("нет телефона {id}"));
                    }
                }
                p
            }
            None => known.iter().filter(|p| p.state == "ready").map(|p| p.id).collect(),
        };
        let id = format!("r{:x}", now_ms());
        let dir = self.data.join("runs").join(&id);
        // Телефоны занимаются атомарно: либо все, либо ни одного.
        {
            let mut busy = self.busy.lock().unwrap();
            if let Some((p, r)) = wanted.iter().find_map(|p| busy.get(p).map(|r| (*p, r.clone()))) {
                return Err(format!("телефон {p} занят прогоном {r}"));
            }
            for p in &wanted {
                busy.insert(*p, id.clone());
            }
        }
        if let Err(e) = fs::create_dir_all(&dir) {
            self.release_phones(&id);
            return Err(format!("{}: {e}", dir.display()));
        }
        let run = Arc::new(Run {
            id: id.clone(),
            script_id: meta.id.clone(),
            script_name: meta.name.clone(),
            phones: wanted.clone(),
            started_ms: now_ms(),
            dir: dir.clone(),
            log: Arc::new(RunLog::new(&id, Some(&dir))),
            stop: Arc::new(AtomicBool::new(false)),
            abort: Arc::new(Mutex::new(None)),
            finished: Mutex::new(None),
        });
        self.runs.lock().unwrap().insert(id.clone(), run.clone());
        self.notify(
            json!({"ev": "RunStarted", "run": id, "script": meta.id, "name": meta.name, "phones": wanted}),
        );

        let (me, r) = (self.clone(), run.clone());
        std::thread::Builder::new()
            .name(format!("run-{id}"))
            .spawn(move || {
                let cfg = RunConfig { phones: r.phones.clone(), args: args.clone(), only, timeout, artifacts: Some(r.dir.clone()) };
                let sink: Arc<dyn Sink> = r.log.clone();
                let report = run_script_ex(&source, &r.script_name, &cfg, me.backend.clone(), sink, r.stop.clone(), r.abort.clone());
                let finished = now_ms();
                let doc = json!({
                    "run": r.id, "script": {"id": r.script_id, "name": r.script_name}, "phones": r.phones, "args": args,
                    "started": r.started_ms, "finished": finished, "status": report.status, "report": report.to_json(),
                });
                let _ = fs::write(r.dir.join("report.json"), serde_json::to_string_pretty(&doc).unwrap());
                *r.finished.lock().unwrap() = Some((finished, report.status.clone()));
                me.release_phones(&r.id);
                me.notify(json!({"ev": "RunEnded", "run": r.id, "status": report.status}));
            })
            .map_err(|e| format!("не встал поток прогона: {e}"))?;
        Ok(run)
    }

    fn release_phones(&self, run: &str) {
        self.busy.lock().unwrap().retain(|_, r| r != run);
    }

    /// Просит остановить прогон. `false`, если такого нет или он закончился.
    pub fn stop(&self, id: &str) -> bool {
        match self.get(id) {
            Some(r) if !r.is_finished() => {
                r.stop.store(true, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    /// Хост прерывает прогон ошибкой с причиной (его часть работы не удалась, например перезагрузка роутера по метке
    /// сценария): сценарий падает со статусом `error` и этим текстом на ближайшем действии. `false` — прогона нет или он
    /// закончился.
    pub fn fail(&self, id: &str, message: &str) -> bool {
        match self.get(id) {
            Some(r) if !r.is_finished() => {
                *r.abort.lock().unwrap() = Some(message.to_string());
                true
            }
            _ => false,
        }
    }

    /// Останавливает все идущие прогоны (при выходе демона).
    pub fn stop_all(&self) {
        for r in self.runs.lock().unwrap().values() {
            r.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Ждёт, пока прогон закончится (для тестов и выхода).
    pub fn wait(&self, id: &str, timeout: Duration) -> bool {
        let end = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < end {
            if self.get(id).is_some_and(|r| r.is_finished()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}
