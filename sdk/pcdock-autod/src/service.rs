//! Протокол демона автоматизации поверх общей основы (`pcdock-net`): сценарии, прогоны, прямой эфир событий.
//!
//! Идентификатор сценария во всех сообщениях называется `script` (поле `id` занято номером запроса).
//!
//! Команды (`"op"`): `Phones`, `ScriptList`, `ScriptGet`, `ScriptPut`, `ScriptRename`, `ScriptDelete`, `Run`, `Stop`,
//! `Fail` (`{run, message}`: хост прерывает прогон ошибкой), `Runs`, `RunInfo`, `Subscribe`, `Artifacts`, `Artifact`. Всем клиентам приходят `RunStarted` и `RunEnded`;
//! события прогона (`RunEvent`) приходят подписчикам.

use std::sync::Arc;
use std::time::Duration;

use pcdock_net::{Handler, Hub};
use serde_json::{json, Value};

use crate::backend::{Backend, PhoneInfo};
use crate::runs::Runs;
use crate::store::ScriptStore;

/// Версия протокола (в `Hello`).
pub const PROTOCOL_VERSION: u32 = 1;

/// Тип бинарного пакета: вложение прогона.
pub const KIND_ARTIFACT: u8 = 4;

pub struct Service {
    store: Arc<ScriptStore>,
    runs: Arc<Runs>,
    backend: Arc<dyn Backend>,
}

fn phone_json(p: &PhoneInfo) -> Value {
    json!({"phone": p.id, "state": p.state, "transport": p.transport, "address": p.address, "serial": p.serial,
           "name": p.name, "width": p.screen.map(|s| s.0), "height": p.screen.map(|s| s.1), "rotation": p.rotation})
}

/// Запускает демон: связывает сервис с основой и рассылку уведомлений о прогонах.
pub fn build(
    store: Arc<ScriptStore>,
    runs: Arc<Runs>,
    backend: Arc<dyn Backend>,
    token: Option<String>,
) -> Hub<()> {
    let hub = Hub::new(Service { store, runs: runs.clone(), backend }, token);
    let h = hub.clone();
    runs.set_notify(Box::new(move |v| h.broadcast(|_| true, v)));
    hub
}

impl Handler<()> for Service {
    fn client_data(&self) {}

    fn hello(&self) -> Value {
        json!({"ev": "Hello", "protocol": PROTOCOL_VERSION, "server": format!("pcdock-autod/{}", env!("CARGO_PKG_VERSION")), "role": "autod"})
    }

    fn on_json(&self, hub: &Hub<()>, client: u64, id: Option<u64>, msg: Value) {
        let op = msg["op"].as_str().unwrap_or("").to_string();
        let reply = |v: Value| hub.reply(client, id, v);
        let fail = |e: String| hub.error(client, id, e);
        let str_of = |k: &str| msg[k].as_str().map(String::from);
        match op.as_str() {
            "Phones" => {
                let phones = self.backend.phones();
                reply(json!({"ev": "Phones", "phones": phones.iter().map(|p| p.id).collect::<Vec<_>>(),
                             "details": phones.iter().map(phone_json).collect::<Vec<_>>()}))
            }
            "ScriptList" => match self.store.list() {
                Ok(list) => reply(json!({"ev": "Scripts", "scripts": list.iter().map(|m| json!({
                    "script": m.id, "name": m.name, "created": m.created_ms, "updated": m.updated_ms, "size": m.size,
                })).collect::<Vec<_>>()})),
                Err(e) => fail(e),
            },
            "ScriptGet" => match self.store.get(&str_of("script").unwrap_or_default()) {
                Ok((m, source)) => reply(
                    json!({"ev": "Script", "script": m.id, "name": m.name, "source": source, "updated": m.updated_ms}),
                ),
                Err(e) => fail(e),
            },
            "ScriptPut" => {
                let (Some(name), Some(source)) = (str_of("name"), str_of("source")) else {
                    return fail("плохая команда: нужны name и source".into());
                };
                match self.store.put(str_of("script").as_deref(), &name, &source) {
                    Ok(m) => reply(json!({"ev": "ScriptSaved", "script": m.id, "name": m.name})),
                    Err(e) => fail(e),
                }
            }
            "ScriptRename" => match self
                .store
                .rename(&str_of("script").unwrap_or_default(), &str_of("name").unwrap_or_default())
            {
                Ok(()) => reply(json!({"ev": "Ok"})),
                Err(e) => fail(e),
            },
            "ScriptDelete" => match self.store.delete(&str_of("script").unwrap_or_default()) {
                Ok(()) => reply(json!({"ev": "Ok"})),
                Err(e) => fail(e),
            },
            "Run" => {
                let Some(script) = str_of("script") else {
                    return fail("плохая команда: нужен script".into());
                };
                let phones = msg["phones"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as u32)).collect::<Vec<u32>>());
                let only = msg["tests"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
                let timeout = msg["timeout"].as_f64().filter(|t| *t > 0.0).map(Duration::from_secs_f64);
                let args = if msg["args"].is_null() { json!({}) } else { msg["args"].clone() };
                match self.runs.start(&script, phones, args, only, timeout) {
                    Ok(run) => {
                        reply(
                            json!({"ev": "RunStarted", "run": run.id, "script": run.script_id, "name": run.script_name, "phones": run.phones}),
                        );
                        // По умолчанию запустивший сразу смотрит прогон с начала.
                        if msg["follow"].as_bool().unwrap_or(true) {
                            self.subscribe(hub, client, &run.id, 0);
                        }
                    }
                    Err(e) => fail(e),
                }
            }
            "Stop" => {
                if self.runs.stop(&str_of("run").unwrap_or_default()) {
                    reply(json!({"ev": "Ok"}))
                } else {
                    fail("нет идущего прогона".into())
                }
            }
            "Fail" => {
                let why = str_of("message").unwrap_or_else(|| "прогон прерван хостом".into());
                if self.runs.fail(&str_of("run").unwrap_or_default(), &why) {
                    reply(json!({"ev": "Ok"}))
                } else {
                    fail("нет идущего прогона".into())
                }
            }
            "Runs" => reply(json!({"ev": "Runs", "runs": self.runs.list()})),
            "RunInfo" => {
                let run = str_of("run").unwrap_or_default();
                match (self.runs.get(&run).map(|r| r.summary()), self.runs.report(&run)) {
                    (summary, report) if summary.is_some() || report.is_some() => reply(
                        json!({"ev": "Run", "run": run, "summary": summary, "report": report, "artifacts": self.runs.artifacts(&run)}),
                    ),
                    _ => fail(format!("нет прогона {run}")),
                }
            }
            "Subscribe" => {
                let run = str_of("run").unwrap_or_default();
                if self.runs.get(&run).is_none() {
                    return fail(format!("нет прогона {run}"));
                }
                self.subscribe(hub, client, &run, msg["from"].as_u64().unwrap_or(0));
                reply(json!({"ev": "Ok"}))
            }
            "Artifacts" => reply(
                json!({"ev": "Artifacts", "run": str_of("run"), "names": self.runs.artifacts(&str_of("run").unwrap_or_default())}),
            ),
            "Artifact" => {
                let (run, name) = (str_of("run").unwrap_or_default(), str_of("name").unwrap_or_default());
                match self.runs.artifact(&run, &name) {
                    Ok(data) => {
                        reply(json!({"ev": "Artifact", "run": run, "name": name, "size": data.len()}));
                        // Данные идут следом отдельным пакетом: `[длина имени u8][имя][содержимое]`.
                        let mut payload = vec![name.len() as u8];
                        payload.extend_from_slice(name.as_bytes());
                        payload.extend_from_slice(&data);
                        hub.with_clients(|m| {
                            if let Some(c) = m.get(&client) {
                                c.out.bulk(KIND_ARTIFACT, payload);
                            }
                        });
                    }
                    Err(e) => fail(e),
                }
            }
            other => fail(format!("неизвестная команда {other:?}")),
        }
    }
}

impl Service {
    /// Подписывает клиента на события прогона с номера `from`: история, затем живой поток.
    fn subscribe(&self, hub: &Hub<()>, client: u64, run: &str, from: u64) {
        let Some(r) = self.runs.get(run) else { return };
        let hub = hub.clone();
        r.log.subscribe(
            from,
            Box::new(move |ev| {
                hub.with_clients(|m| m.get(&client).is_some_and(|c| c.out.json(ev.to_string())))
            }),
        );
    }
}
