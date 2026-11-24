//! Play a newline-delimited test script against the real desktop stdio link.
#[path = "../synth.rs"]
mod synth;
use serde_json::Value;
use std::{
    collections::HashMap,
    env, fs,
    io::{self, BufRead, Write},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args_os()
        .nth(1)
        .ok_or("usage: fake-app-server SCRIPT")?;
    if path == "--synthetic" {
        return Ok(synth::run()?);
    }
    let mut input = io::stdin().lock().lines();
    let mut output = io::stdout().lock();
    let mut ids = HashMap::new();
    for (index, line) in fs::read_to_string(path)?.lines().enumerate() {
        let step: Value = serde_json::from_str(line)?;
        if let Some(expect) = step["expect"].as_object() {
            let request: Value =
                serde_json::from_str(&input.next().ok_or("client closed early")??)?;
            for (key, value) in expect {
                if request.get(key) != Some(value) {
                    return Err(format!("unexpected {key} in step {}", index + 1).into());
                }
            }
            if let Some(name) = step["capture"].as_str() {
                ids.insert(
                    name.to_owned(),
                    request.get("id").ok_or("missing id")?.clone(),
                );
            }
        } else if let Some(mut frame) = step.get("send").cloned() {
            if let Some(name) = step["replyTo"].as_str() {
                frame["id"] = ids.get(name).ok_or("unbound request")?.clone();
            }
            serde_json::to_writer(&mut output, &frame)?;
            output.write_all(b"\n")?;
            output.flush()?;
        } else {
            return Err(format!("unknown step {}", index + 1).into());
        }
    }
    Ok(())
}
