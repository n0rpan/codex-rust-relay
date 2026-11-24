//! Decode only the OX worker MCP result fields used for display and control.
use serde::Deserialize;
use serde_json::Value;

use crate::ports::ox_workers::Snapshot;
#[derive(Clone, Debug)]
pub struct Observation {
    pub title: Option<String>,
    pub snapshot: Snapshot,
}

#[derive(Debug)]
pub enum Response {
    Snapshot(Box<Snapshot>),
    Accepted,
}
pub fn result(value: &Value) -> Result<Response, String> {
    if value["isError"] == true {
        return Err(value
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("OX worker unavailable")
            .into());
    }
    fn decode(value: &Value) -> Option<Response> {
        if value["accepted"] == true {
            Some(Response::Accepted)
        } else {
            Snapshot::deserialize(value)
                .ok()
                .map(|s| Response::Snapshot(Box::new(s)))
        }
    }
    if let Some(data) = value.get("structuredContent").and_then(decode) {
        return Ok(data);
    }
    value["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|c| {
            let parsed = serde_json::from_str::<Value>(c["text"].as_str()?).ok()?;
            decode(&parsed)
        })
        .ok_or_else(|| "Invalid OX worker reply".into())
}

impl Observation {
    pub fn decode(item: &Value) -> Option<Box<Self>> {
        if item["type"] != "mcpToolCall"
            || item["server"] != "ox_workers"
            || item["status"] != "completed"
        {
            return None;
        }
        let title = match item["tool"].as_str()? {
            "start_ox_worker" => Some(
                item.pointer("/arguments/task")
                    .and_then(Value::as_str)
                    .unwrap_or("OX worker")
                    .lines()
                    .next()
                    .unwrap_or("OX worker")
                    .chars()
                    .take(100)
                    .collect(),
            ),
            "ox_worker_status" | "cancel_ox_worker" | "steer_ox_worker" => None,
            _ => return None,
        };
        let Response::Snapshot(snapshot) = result(&item["result"]).ok()? else {
            return None;
        };
        Some(Box::new(Self {
            title,
            snapshot: *snapshot,
        }))
    }
}
