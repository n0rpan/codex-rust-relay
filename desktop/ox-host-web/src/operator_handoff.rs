//! Keeps the host handoff receipt private to this operator. Resume only after the PTY exits.
use super::{App, bridge};
use ox_desk_core::{
    actions::Action,
    host_protocol::{Client, Scope, Server, VERSION},
};
use ox_pty::{Screen, TerminalCommand};
use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::BufReader, net::UnixStream, time::timeout};

pub(super) struct Lease {
    peer: Option<Peer>,
    key: String,
    scope: Scope,
    thread: String,
    argv: Vec<String>,
    requested: bool,
    handed_off: bool,
    process: Option<(Arc<Mutex<Screen>>, flume::Receiver<()>)>,
}
impl Lease {
    pub async fn preview(
        app: &App,
        scope: Scope,
        thread: String,
    ) -> io::Result<(Self, TerminalCommand)> {
        let mut peer = Peer {
            conn: BufReader::new(bridge::connect(&app.host).await?),
            buf: vec![],
        };
        bridge::write(peer.conn.get_mut(), &Client::Hello { version: VERSION }).await?;
        let Hello { projects, routes } = hello(&mut peer).await?;
        if !(projects.contains(&scope.project) && routes.contains(&scope.route))
            && !app.discovered.lock().unwrap().contains(&scope)
        {
            return Err(io::Error::other("Unavailable session scope"));
        }
        bridge::write(
            peer.conn.get_mut(),
            &Client::Subscribe {
                scope: Some(scope.clone()),
                after: None,
                terminal: false,
            },
        )
        .await?;
        bridge::write(
            peer.conn.get_mut(),
            &Client::TerminalPreview {
                id: "preview".into(),
                scope: scope.clone(),
                thread: thread.clone(),
            },
        )
        .await?;
        let argv = loop {
            match frame(&mut peer).await? {
                Server::TerminalPreview {
                    id,
                    scope: s,
                    thread: t,
                    argv,
                } if id == "preview" && s == scope && t == thread => break argv,
                Server::Error { message, .. } => return Err(io::Error::other(message)),
                _ => (),
            }
        };
        let command = TerminalCommand {
            program: super::remote_settings::binary().map_err(io::Error::other)?,
            args: argv.iter().map(Into::into).collect(),
        };
        Ok((
            Self {
                peer: Some(peer),
                key: crate::state::random(),
                scope,
                thread,
                argv,
                requested: false,
                handed_off: false,
                process: None,
            },
            command,
        ))
    }
    pub async fn acquire(&mut self) -> io::Result<()> {
        self.requested = true;
        let peer = self.peer.as_mut().expect("live lease");
        let action = Action::TerminalText(Some(self.thread.clone()), "/terminal".into());
        bridge::write(
            peer.conn.get_mut(),
            &Client::Act {
                id: self.key.clone(),
                scope: self.scope.clone(),
                action,
                approval: None,
            },
        )
        .await?;
        match receipt(peer, &self.scope, &self.thread, &self.key).await {
            Ok(actual) => {
                self.handed_off = true;
                if actual != self.argv {
                    return Err(io::Error::other("The native session route changed"));
                }
                Ok(())
            }
            Err(e) => {
                if e.kind() == io::ErrorKind::PermissionDenied {
                    self.requested = false;
                }
                Err(e)
            }
        }
    }
    pub fn process(&mut self, screen: Arc<Mutex<Screen>>, updates: flume::Receiver<()>) {
        self.process = Some((screen, updates));
    }
}
struct Peer {
    conn: BufReader<UnixStream>,
    buf: Vec<u8>,
}
async fn frame(peer: &mut Peer) -> io::Result<Server> {
    Ok(serde_json::from_slice(
        &bridge::line(&mut peer.conn, &mut peer.buf).await?,
    )?)
}
struct Hello {
    projects: Vec<PathBuf>,
    routes: Vec<ox_desk_core::runtime::Route>,
}
async fn hello(peer: &mut Peer) -> io::Result<Hello> {
    match frame(peer).await? {
        Server::Hello {
            projects,
            routes,
            version,
            ..
        } if ox_desk_core::host_protocol::supported(version) => Ok(Hello { projects, routes }),
        _ => Err(io::Error::other("Incompatible host")),
    }
}
async fn receipt(
    peer: &mut Peer,
    scope: &Scope,
    thread: &str,
    key: &str,
) -> io::Result<Vec<String>> {
    loop {
        match frame(peer).await? {
            Server::Handoff {
                id,
                scope: s,
                thread: Some(t),
                argv,
                ..
            } if id == key && &s == scope && t == thread => return Ok(argv),
            Server::Error {
                id: Some(id),
                message,
            } if id == key => return Err(io::Error::new(io::ErrorKind::PermissionDenied, message)),
            _ => (),
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if !self.requested {
            return;
        }
        let Some(mut peer) = self.peer.take() else {
            return;
        };
        let (scope, thread, key, received, process) = (
            self.scope.clone(),
            self.thread.clone(),
            self.key.clone(),
            self.handed_off,
            self.process.take(),
        );
        tokio::spawn(async move {
            if !received
                && !matches!(
                    timeout(
                        Duration::from_secs(20),
                        receipt(&mut peer, &scope, &thread, &key)
                    )
                    .await,
                    Ok(Ok(_))
                )
            {
                return;
            }
            if let Some((screen, events)) = process {
                while screen.lock().unwrap().outcome.is_none() {
                    if events.recv_async().await.is_err() {
                        return;
                    }
                }
            }
            let _ = bridge::write(
                peer.conn.get_mut(),
                &Client::Act {
                    id: crate::state::random(),
                    scope,
                    action: Action::ResumeHere(thread),
                    approval: None,
                },
            )
            .await;
        });
    }
}
