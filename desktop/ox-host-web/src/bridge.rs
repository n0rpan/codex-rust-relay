use crate::{
    server::{App, Proof},
    state::Purpose,
};
use axum::extract::ws::{Message, WebSocket};
use ox_desk_core::{
    actions::Action,
    host_protocol::{Client, Scope, Server, VERSION},
    runtime::Route,
};
use serde::Serialize;
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};

pub(crate) async fn connect(path: &Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path).await?;
    if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other("Invalid host peer"));
    }
    Ok(stream)
}
pub(crate) async fn write<T: Serialize>(
    w: &mut (impl AsyncWrite + Unpin),
    v: &T,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(v)?;
    bytes.push(b'\n');
    w.write_all(&bytes).await
}
/// Keep the partial frame in the caller. Cancellation (including inbound WS traffic) loses no bytes.
pub(crate) async fn line(
    reader: &mut (impl AsyncBufRead + Unpin),
    buf: &mut Vec<u8>,
) -> io::Result<Vec<u8>> {
    loop {
        let part = reader.fill_buf().await?;
        if part.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let end = part.iter().position(|&b| b == b'\n');
        let len = end.unwrap_or(part.len());
        if buf.len() + len > 64 * 1024 * 1024 {
            return Err(io::Error::other("Oversized session-host frame"));
        }
        buf.extend_from_slice(&part[..len]);
        reader.consume(len + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(std::mem::take(buf));
        }
    }
}
// Attach only bytes supplied by this device. Local paths refer to files on the host.
fn valid_upload(image: &ox_desk_core::mentions::ImageInput) -> bool {
    let ox_desk_core::mentions::ImageInput::Clipboard(mime, data) = image else {
        return false;
    };
    matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) && data
        .strip_prefix(&format!("data:{mime};base64,"))
        .is_some_and(|b64| {
            !b64.is_empty()
                && b64.len() <= 8 * 1024 * 1024
                && b64
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        })
}
#[derive(Default)]
struct Access {
    hello: bool,
    ready: bool,
    projects: HashSet<PathBuf>,
    routes: HashSet<Route>,
    selected: Option<Scope>,
    discovery: Option<String>,
}
impl Access {
    fn host(&mut self, app: &App, bytes: &[u8]) -> io::Result<()> {
        if !self.ready {
            match serde_json::from_slice::<Server>(bytes)? {
                Server::Hello {
                    version,
                    projects,
                    routes,
                    ..
                } if self.hello && version == VERSION => {
                    self.ready = true;
                    self.projects = projects.into_iter().collect();
                    self.routes = routes.into_iter().collect();
                }
                Server::Error { .. } => {}
                _ => return Err(io::Error::other("Incompatible session host")),
            }
        } else if self.discovery.is_some() {
            match serde_json::from_slice::<Server>(bytes)? {
                Server::Discovered { id, scope, .. } if Some(&id) == self.discovery.as_ref() => {
                    let mut known = app.discovered.lock().unwrap();
                    if known.len() < 8192 {
                        known.insert(scope);
                    }
                }
                Server::DiscoveredEnd { id, .. } if Some(&id) == self.discovery.as_ref() => {
                    self.discovery = None
                }
                _ => {}
            }
        }
        Ok(())
    }
    /// A start request that passed auth and scope but failed validation: the request id and
    /// the reason. Protocol, auth and scope violations return `None` and still close the socket.
    fn invalid(&self, req: &Client) -> Option<(String, String)> {
        let Client::Act {
            id,
            scope,
            action,
            approval,
        } = req
        else {
            return None;
        };
        if !self.ready
            || self.selected.as_ref() != Some(scope)
            || id.is_empty()
            || id.len() > 96
            || approval.is_some()
        {
            return None;
        }
        let message = match action {
            Action::StartWorker(start) => start.validate().err()?,
            Action::StartHandoff(start) => start.validate().err()?,
            _ => return None,
        };
        Some((id.clone(), message))
    }
    fn permit(&mut self, app: &App, req: &Client) -> bool {
        match req {
            Client::Hello { version } if !self.hello && *version == VERSION => {
                self.hello = true;
                true
            }
            Client::Discover { id }
                if self.ready && self.discovery.is_none() && id.len() <= 96 && !id.is_empty() =>
            {
                self.discovery = Some(id.clone());
                true
            }
            Client::WatchCatalog if self.ready => {
                self.selected = None;
                true
            }
            Client::Subscribe { scope, .. } if self.ready => {
                if scope.as_ref().is_some_and(|s| {
                    (!self.projects.contains(&s.project) || !self.routes.contains(&s.route))
                        && !app.discovered.lock().unwrap().contains(s)
                }) {
                    return false;
                }
                self.selected = scope.clone();
                true
            }
            Client::Act {
                id,
                scope,
                action,
                approval,
            } if self.ready
                && self.selected.as_ref() == Some(scope)
                && id.len() <= 96
                && !id.is_empty() =>
            {
                match action {
                    Action::SelectThread(_)
                    | Action::GenerateTitle(_)
                    | Action::RenameThread(..)
                    | Action::ArchiveThread(_)
                    | Action::ForkThread(_)
                    | Action::ReadAccountLimits
                    | Action::NativeCommand(..)
                    | Action::ModelOptions(..)
                    | Action::LoadOlder(_)
                    | Action::Stop(_)
                    | Action::SendText(..) => approval.is_none(),
                    Action::StartWorker(start) => approval.is_none() && start.validate().is_ok(),
                    Action::StartHandoff(start) => approval.is_none() && start.validate().is_ok(),
                    Action::YoloGrant(id, _) => {
                        approval.is_none() && !id.is_empty() && id.len() <= 512
                    }
                    Action::Mention(_, query) => {
                        approval.is_none() && query.as_ref().is_none_or(|q| q.len() <= 512)
                    }
                    Action::SendImages(_, text, images) => {
                        approval.is_none()
                            && text.len() <= 128 * 1024
                            && !images.is_empty()
                            && images.len() <= 4
                            && images.iter().all(valid_upload)
                    }
                    Action::SetNextAccess(mode) | Action::SetAccess(_, mode) => {
                        approval.is_none()
                            && ox_desk_core::runtime::Access::choices(scope.route.runtime)
                                .contains(mode)
                    }
                    Action::AnswerAsk(..) | Action::AnswerQuestions(..) => approval
                        .as_ref()
                        .is_some_and(|s| !s.is_empty() && s.len() <= 256),
                    _ => false,
                }
            }
            _ => false,
        }
    }
}
pub(crate) async fn session(app: App, id: String, mut ws: WebSocket) {
    let Ok(Some(Ok(Message::Text(frame)))) = timeout(Duration::from_secs(5), ws.recv()).await
    else {
        return;
    };
    let Ok(proof) = serde_json::from_str::<Proof>(&frame) else {
        return;
    };
    let mut changes = {
        let mut s = app.state.lock().unwrap();
        if !s.prove(&id, Purpose::Connect, &proof.nonce, &proof.signature) {
            return;
        }
        s.changed.subscribe()
    };
    let Ok(host) = connect(&app.host).await else {
        return;
    };
    let (r, mut w) = host.into_split();
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    let mut access = Access::default();
    loop {
        if !app.state.lock().unwrap().active_device(&id) {
            break;
        }
        tokio::select! {biased;
            _=changes.changed()=>{},
            msg=ws.recv()=>{
                let Some(Ok(msg))=msg else{break};
                match msg{
                    Message::Text(s)=>{
                        let Ok(req)=serde_json::from_str::<Client>(&s) else{break};
                        if !access.permit(&app,&req) {
                            // Answer the refused request with its reason and keep the socket open.
                            let Some((request,message))=access.invalid(&req) else{break};
                            if !app.state.lock().unwrap().alive(&id){break}
                            let Ok(frame)=serde_json::to_string(&Server::Error{id:Some(request),message}) else{break};
                            if !matches!(timeout(Duration::from_secs(8),ws.send(Message::Text(frame.into()))).await,Ok(Ok(()))){break}
                            continue;
                        }
                        if !app.state.lock().unwrap().alive(&id){break}
                        if !matches!(timeout(Duration::from_secs(5),write(&mut w,&req)).await,Ok(Ok(()))){break}
                    }
                    Message::Ping(_) | Message::Pong(_)=>{},
                    _=>break
                }
            },
            frame=line(&mut r,&mut buf)=>{
                let Ok(bytes)=frame else{break};
                if access.host(&app,&bytes).is_err() || !app.state.lock().unwrap().alive(&id){break}
                let Ok(s)=String::from_utf8(bytes) else{break};
                if !matches!(timeout(Duration::from_secs(8),ws.send(Message::Text(s.into()))).await,Ok(Ok(()))){break}
            },
        }
    }
    let _ = timeout(Duration::from_secs(1), ws.send(Message::Close(None))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_actions_use_subscribed_route_and_native_permission_modes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let app = App {
            state: std::sync::Arc::new(std::sync::Mutex::new(
                crate::state::State::load(dir.path()).unwrap(),
            )),
            host: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            discovered: Default::default(),
        };
        let mut a = Access::default();
        let route = Route::CODEX_SUBSCRIPTION;
        let scope = Scope {
            project: "/approved".into(),
            route: route.clone(),
        };
        let mut other = scope.clone();
        other.project = "/other".into();
        assert!(a.permit(&app, &Client::Hello { version: VERSION }));
        let hello = Server::Hello {
            version: VERSION,
            projects: vec![scope.project.clone()],
            routes: vec![route],
            capabilities: vec![],
            host: None,
        };
        a.host(&app, &serde_json::to_vec(&hello).unwrap()).unwrap();
        assert!(!a.permit(
            &app,
            &Client::Subscribe {
                scope: Some(other.clone()),
                after: None
            }
        ));
        assert!(a.permit(
            &app,
            &Client::Subscribe {
                scope: Some(scope.clone()),
                after: None
            }
        ));
        let act = |s: Scope, action, approval| Client::Act {
            id: "request".into(),
            scope: s,
            action,
            approval,
        };
        assert!(!a.permit(&app, &act(other, Action::Stop("thread".into()), None)));
        assert!(a.permit(
            &app,
            &act(
                scope.clone(),
                Action::SetNextAccess(ox_desk_core::runtime::Access::Yolo),
                None
            )
        ));
        assert!(!a.permit(
            &app,
            &act(
                scope.clone(),
                Action::SetNextAccess(ox_desk_core::runtime::Access::BypassPermissions),
                None
            )
        ));
        assert!(!a.permit(
            &app,
            &act(
                scope.clone(),
                Action::PaletteCommand(None, "/terminal".into()),
                None
            )
        ));
        assert!(!a.permit(&app, &act(scope.clone(), Action::Terminal(None), None)));
        assert!(!a.permit(
            &app,
            &act(
                scope,
                Action::AnswerAsk(ox_desk_core::runtime::RequestId::Number(1), 0),
                None
            )
        ));
    }

    #[test]
    fn refused_start_answers_its_request_and_keeps_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let app = App {
            state: std::sync::Arc::new(std::sync::Mutex::new(
                crate::state::State::load(dir.path()).unwrap(),
            )),
            host: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            discovered: Default::default(),
        };
        let route = Route::CODEX_SUBSCRIPTION;
        let scope = Scope {
            project: "/approved".into(),
            route: route.clone(),
        };
        let blank_handoff = || Client::Act {
            id: "handoff-1".into(),
            scope: scope.clone(),
            action: Action::StartHandoff(ox_desk_core::handoff::HandoffStart {
                source: "thread".into(),
                runtime: route.runtime,
                model: None,
                mode: Default::default(),
                request: "  \n".into(),
                yolo_confirmed: false,
            }),
            approval: None,
        };

        // Before the device has said hello and subscribed, nothing is answered: close.
        let mut a = Access::default();
        assert!(a.invalid(&blank_handoff()).is_none());

        assert!(a.permit(&app, &Client::Hello { version: VERSION }));
        let hello = Server::Hello {
            version: VERSION,
            projects: vec![scope.project.clone()],
            routes: vec![route.clone()],
            capabilities: vec![],
            host: None,
        };
        a.host(&app, &serde_json::to_vec(&hello).unwrap()).unwrap();
        assert!(a.permit(
            &app,
            &Client::Subscribe {
                scope: Some(scope.clone()),
                after: None
            }
        ));

        // The refused start is a validation failure: it gets the reason keyed by its id.
        let req = blank_handoff();
        assert!(!a.permit(&app, &req));
        let (id, message) = a.invalid(&req).expect("validation refusal is answered");
        assert_eq!(id, "handoff-1");
        assert_eq!(message, "Write the request for the new session.");
        let frame = serde_json::to_value(Server::Error {
            id: Some(id),
            message,
        })
        .unwrap();
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["id"], "handoff-1");

        // A start that carries an approval is a protocol violation: close, do not answer.
        let mut approved = blank_handoff();
        if let Client::Act { approval, .. } = &mut approved {
            *approval = Some("yes".into());
        }
        assert!(!a.permit(&app, &approved));
        assert!(a.invalid(&approved).is_none());
    }
}
