//! Enumerate installed native profiles on behalf of all host clients.
use crate::{
    claude::catalog::Profiles,
    codex::{frame::Frame, reply::Reply, request::Call},
    host_protocol::{Scope, SessionView},
    links::{LinkEvent, codex::Link},
    ports::ox,
    runtime::{Route, Runtime},
    settings_protocol as protocol,
    store::{ThreadRow, claude::saved_rows},
    titles::Index,
};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    io,
    path::Path,
    process::Command,
    sync::mpsc,
    time::Duration,
};

pub fn discover(
    ox: &Path,
    path: &OsStr,
    claude_index: &Path,
    titles: &Path,
    mut emit: impl FnMut(Scope, Vec<SessionView>) -> bool,
) -> Vec<String> {
    let mut errors = Vec::new();
    let index = Index::load(titles).unwrap_or_default();
    let settings = ox::snapshot(ox, path);
    let reduced = settings
        .as_ref()
        .map(|s| s.models.as_slice())
        .unwrap_or_default();
    // Saved history on an alias nobody starred is still that alias's session: validate against
    // the full catalog, asked once and only when a row names an alias the reduced list lacks.
    let full = std::cell::OnceCell::<Vec<String>>::new();
    let codex_provider = settings
        .as_ref()
        .ok()
        .and_then(|s| s.api_provider.as_deref());
    let mut publish = |runtime: Runtime, api: bool, rows: Vec<ThreadRow>| {
        let mut groups = HashMap::<Scope, Vec<SessionView>>::new();
        for mut row in rows.into_iter().filter(|r| r.cwd.is_absolute()) {
            index.decorate(&mut row);
            let known_in = |aliases: &[String]| match runtime {
                Runtime::Claude => row.saved_runtime_route(aliases),
                Runtime::Codex if !api && row.model_provider == "openai" => {
                    Some(Route::CODEX_SUBSCRIPTION)
                }
                Runtime::Codex if api && Some(row.model_provider.as_str()) == codex_provider => row
                    .model
                    .as_ref()
                    .filter(|alias| aliases.contains(alias))
                    .cloned()
                    .map(Route::codex_api),
                _ => None,
            };
            let known = known_in(reduced).or_else(|| {
                row.model.as_ref()?;
                known_in(full.get_or_init(|| ox::catalog(ox, path, reduced)))
            });
            let available = known.is_some();
            // Unknown providers remain visible. The host must verify the binding before opening.
            let route = known.unwrap_or_else(|| {
                runtime.route(api.then(|| row.model.clone().unwrap_or_default()))
            });
            let owner = Scope {
                project: row.cwd.clone(),
                route,
            };
            groups.entry(owner).or_default().push(SessionView{id:row.id.clone(),title:row.title().into(),parent:row.parent_thread_id.clone(),status:"saved".into(),updated_at:row.updated_at,capabilities:if available{vec!["open".into()]}else{vec![]},detail:json!({"row":row,"generated":row.generated_title,"available":available,"owned":false,"worker":row.worker})});
        }
        groups.into_iter().all(|(scope, rows)| emit(scope, rows))
    };
    match ox::request::<protocol::ClaudeProfiles>(ox, path, protocol::Request::ClaudeProfiles)
        .and_then(|cfg| {
            saved_rows(
                &Profiles {
                    subscription: cfg.subscription,
                    api: cfg.api,
                },
                claude_index,
            )
            .map_err(|e| e.to_string())
        }) {
        Ok(rows) => {
            let (api, native) = rows
                .into_iter()
                .partition(|r: &ThreadRow| r.saved_claude_api() == Some(true));
            if !publish(Runtime::Claude, false, native) || !publish(Runtime::Claude, true, api) {
                return errors;
            }
        }
        Err(e) => errors.push(e),
    }
    match ox::request::<protocol::CodexProfiles>(ox, path, protocol::Request::CodexProfiles) {
        Ok(homes) => {
            for (api, home) in [(false, homes.subscription), (true, homes.api)] {
                if let Some(home) = home
                    && let Err(e) =
                        codex_rows(path, &home, api, |rows| publish(Runtime::Codex, api, rows))
                {
                    errors.push(e);
                }
            }
        }
        Err(e) => errors.push(e),
    }
    errors
}
fn codex_rows(
    path: &OsStr,
    home: &Path,
    api: bool,
    emit: impl FnMut(Vec<ThreadRow>) -> bool,
) -> Result<(), String> {
    let binary = std::env::split_paths(path)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("codex"))
        .find(|p| p.is_file())
        .ok_or("Install Codex to discover its saved sessions.")?;
    let mut command = Command::new(binary);
    // A catalog never sends turns or needs provider credentials. The native profile can use
    // a custom SQLite location. The OX relay always pins SQLite to its isolated profile.
    command
        .env_clear()
        .env("PATH", path)
        .env("CODEX_HOME", home);
    for key in [
        "HOME",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if api {
        command
            .arg("-c")
            .arg(format!("sqlite_home={}", serde_json::json!(home)));
    } else if let Some(custom) = std::env::var_os("CODEX_SQLITE_HOME") {
        command.env("CODEX_SQLITE_HOME", custom);
    }
    command
        .current_dir(Path::new("/"))
        .args(["app-server", "--listen", "stdio://"]);
    collect(command, emit).map_err(|e| e.to_string())
}

fn collect(command: Command, mut emit: impl FnMut(Vec<ThreadRow>) -> bool) -> io::Result<()> {
    let (send, receive) = mpsc::channel();
    let link = Link::from_command(command, move |e| {
        let _ = send.send(e);
    })?;
    let mut seq = 1;
    let response = |seq| loop {
        match receive
            .recv_timeout(Duration::from_secs(15))
            .map_err(io::Error::other)?
        {
            LinkEvent::Frame(Frame::Reply {
                id: crate::runtime::RequestId::Number(id),
                result,
            }) if id == seq => return result.map_err(|e| io::Error::other(e.message)),
            LinkEvent::Closed { reason, .. } => return Err(io::Error::other(reason)),
            _ => (),
        }
    };
    link.request(seq, Call::Initialize)?;
    match response(seq)? {
        Reply::Acknowledged => link.initialized()?,
        Reply::UnsupportedVersion(message) => return Err(io::Error::other(message)),
        _ => return Err(io::Error::other("Unexpected Codex initialization response")),
    }
    let mut seen = HashSet::new();
    let mut cursor = None;
    loop {
        seq += 1;
        link.request(seq, Call::WorkspaceList(cursor.take()))?;
        let Reply::Threads(page) = response(seq)? else {
            return Err(io::Error::other("Unexpected Codex catalog response"));
        };
        if !emit(page.data) {
            break;
        }
        cursor = page.next_cursor.filter(|c| seen.insert(c.clone()));
        if cursor.is_none() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_requests_only_paged_metadata_without_a_cwd_or_account() {
        let mut child = Command::new("python3");
        child.args([
            "-c",
            r#"
import json, sys
def read(): return json.loads(sys.stdin.readline())
def reply(req, result): print(json.dumps({'id':req['id'],'result':result}),flush=True)
r=read()
assert r['method']=='initialize'
reply(r, {'userAgent':'codex/0.171.0'})
assert read()['method']=='initialized'
for cursor in (None, 'next'):
    r=read()
    assert r['method']=='thread/list' and 'cwd' not in r['params'] and r['params']['cursor']==cursor
    good={'id':str(cursor),'cwd':'/beta' if cursor else '/alpha','modelProvider':'external-vendor'}
    reply(r, {'data':[{'id':'corrupt'},good],'nextCursor':'next'})
"#,
        ]);
        let mut rows = Vec::new();
        collect(child, |p| {
            rows.extend(p);
            true
        })
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].cwd, Path::new("/alpha"));
        assert_eq!(rows[1].cwd, Path::new("/beta"));
        assert_eq!(rows[1].model_provider, "external-vendor");
    }
}
