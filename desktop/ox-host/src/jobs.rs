//! Routes controls to the authenticated loopback OX MCP that actually started
//! each job. Capability tokens live only in this process, never in a projection.
use crate::engine::Message;
use ox_desk_core::{
    codex::jobs::{Response, result},
    host_protocol::Scope,
    ports::ox_workers::Snapshot,
    worker::WorkerAction,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, Read},
    sync::{Arc, Mutex, mpsc::SyncSender},
    thread,
    time::Duration,
};

#[derive(Clone, Deserialize)]
pub(crate) struct Registration {
    port: u16,
    token: String,
    pub snapshot: Snapshot,
}
#[derive(Clone)]
struct Gateway {
    write: bool,
    claude: bool,
    port: u16,
    token: String,
    number: u64,
}
impl Gateway {
    fn call(&self, operation: &WorkerAction) -> Result<Response, String> {
        let (name, args) = match operation {
            WorkerAction::Refresh => ("ox_worker_status", json!({"job_id":self.number})),
            WorkerAction::Stop => ("cancel_ox_worker", json!({"job_id":self.number})),
            WorkerAction::Steer(text) => {
                ("steer_ox_worker", json!({"job_id":self.number,"text":text}))
            }
        };
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(35))
            .build()
            .map_err(|_| "Cannot initialize local worker controls")?;
        let reply=client.post(format!("http://127.0.0.1:{}/mcp",self.port)).bearer_auth(&self.token)
            .header("Content-Type","application/json").body(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}).to_string())
            .send().and_then(|r|r.error_for_status()).map_err(|_|"The OX worker service closed")?;
        let body: Value = serde_json::from_reader(reply.take(1024 * 1024))
            .map_err(|_| "Invalid OX worker response")?;
        result(&body["result"])
    }
}
#[derive(Clone, Default)]
pub(crate) struct Gateways(Arc<Mutex<HashMap<String, Gateway>>>);
impl Gateways {
    pub fn attach(
        &self,
        key: String,
        data: &Registration,
        write: bool,
        claude: bool,
    ) -> io::Result<()> {
        if data.port == 0
            || data.token.len() < 16
            || data.token.len() > 512
            || data.snapshot.job_id == 0
        {
            return Err(io::Error::other("Invalid worker registration"));
        }
        let mut map = self.0.lock().unwrap();
        if map.contains_key(&key) {
            return Err(io::Error::other("Worker already registered"));
        }
        map.insert(
            key,
            Gateway {
                write,
                claude,
                port: data.port,
                token: data.token.clone(),
                number: data.snapshot.job_id,
            },
        );
        Ok(())
    }
    pub fn permits_nested(&self, id: &str, write: bool, claude: bool) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|g| !write || g.write && g.claude == claude)
    }
    pub fn control(
        &self,
        scope: Scope,
        generation: u64,
        tx: SyncSender<Message>,
    ) -> impl Fn(&str, WorkerAction) -> bool + Send + 'static {
        let map = self.clone();
        move |key, action| {
            let Some(service) = map.0.lock().unwrap().get(key).cloned() else {
                return false;
            };
            let id = key.to_owned();
            let scope = scope.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let result = service.call(&action);
                let _ = tx.send(Message::JobReply {
                    scope,
                    generation,
                    id,
                    action,
                    result,
                });
            });
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_writes_require_the_recorded_parent_ceiling_and_matching_sandbox() {
        let gateways = Gateways::default();
        let account:Registration=serde_json::from_value(json!({"port":22,"token":"opaque-fixture-only", "snapshot":{"job_id":1,"runtime":"codex","model":"fixture","status":"running"}})).unwrap();
        gateways
            .attach("read".into(), &account, false, false)
            .unwrap();
        gateways
            .attach("write".into(), &account, true, false)
            .unwrap();
        assert!(gateways.permits_nested("read", false, false));
        assert!(!gateways.permits_nested("read", true, false));
        assert!(gateways.permits_nested("write", true, false));
        assert!(!gateways.permits_nested("write", true, true));
        assert!(!gateways.permits_nested("unknown", false, false));
        assert!(
            gateways
                .attach("read".into(), &account, true, false)
                .is_err()
        );
    }
}
