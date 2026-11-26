use super::*;
use ox_desk_core::host_protocol::{Change, SessionView};
use std::sync::Mutex;

pub(crate) enum CatalogEvent {
    Host(ox_desk_core::host_protocol::HostInfo),
    Reset,
    Disconnected(io::ErrorKind),
    Waiting(Scope, HashSet<String>),
    Archived(Scope, std::collections::HashSet<String>),
    Replace(Scope, Vec<SessionView>),
    Update(Scope, SessionView),
}
pub(crate) struct CatalogWatch {
    // Drop disconnects even a reader asleep on an idle socket.
    socket: Arc<Mutex<Option<UnixStream>>>,
    cancel: mpsc::Sender<()>,
}
impl Drop for CatalogWatch {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
        if let Some(s) = self.socket.lock().unwrap().take() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

impl HostLink {
    pub(crate) fn watch_catalog(
        &self,
        resources: bool,
    ) -> io::Result<(CatalogWatch, flume::Receiver<CatalogEvent>)> {
        let link = self.clone();
        let (tx, rx) = flume::bounded(32);
        let (cancel, cancelled) = mpsc::channel();
        let socket = Arc::new(Mutex::new(None));
        let connection = socket.clone();
        thread::Builder::new()
            .name("ox-host-catalog".into())
            .spawn(move || {
                let read = || -> io::Result<()> {
                    let (mut input, mut stream, caps) = link.handshake()?;
                    {
                        let mut active = connection.lock().unwrap();
                        if cancelled.try_recv() != Err(mpsc::TryRecvError::Empty) {
                            return Ok(());
                        }
                        *active = Some(stream.try_clone()?);
                    }
                    // An installed host can outlive its original desktop binary.
                    let request = if caps.iter().any(|cap| cap == "live_catalog") {
                        Client::WatchCatalog
                    } else {
                        Client::Subscribe {
                            scope: None,
                            after: None,
                            terminal: false,
                        }
                    };
                    write_frame(&mut stream, &request)?;
                    if resources && caps.iter().any(|cap| cap == "host_status") {
                        write_frame(&mut stream, &Client::WatchHost)?;
                    }
                    let emit = |event| tx.send(event).map_err(io::Error::other);
                    loop {
                        match read_frame(&mut input)? {
                            Server::HostStatus { host } => emit(CatalogEvent::Host(host))?,
                            Server::Snapshot { scopes, .. } => {
                                emit(CatalogEvent::Reset)?;
                                for s in scopes {
                                    if let Ok(ids) =
                                        serde_json::from_value(s.detail["archived"].clone())
                                    {
                                        emit(CatalogEvent::Archived(s.scope.clone(), ids))?;
                                    }
                                    let waiting =
                                        serde_json::from_value(s.detail["request_threads"].clone())
                                            .unwrap_or_else(|_| {
                                                s.asks
                                                    .into_iter()
                                                    .filter_map(|ask| ask.thread)
                                                    .collect()
                                            });
                                    emit(CatalogEvent::Waiting(s.scope.clone(), waiting))?;
                                    emit(CatalogEvent::Replace(s.scope, s.sessions))?;
                                }
                            }
                            Server::Delta { scope, changes, .. } => {
                                for change in changes {
                                    match change {
                                        Change::Sessions { sessions } => {
                                            emit(CatalogEvent::Replace(scope.clone(), sessions))?
                                        }
                                        Change::Session { session } => {
                                            emit(CatalogEvent::Update(scope.clone(), session))?
                                        }
                                        Change::Asks { asks } => {
                                            emit(CatalogEvent::Waiting(
                                                scope.clone(),
                                                asks.into_iter().filter_map(|a| a.thread).collect(),
                                            ))?;
                                        }
                                        Change::State { detail, .. } => {
                                            if let Ok(ids) = serde_json::from_value(
                                                detail["request_threads"].clone(),
                                            ) {
                                                emit(CatalogEvent::Waiting(scope.clone(), ids))?;
                                            }
                                            if let Ok(ids) =
                                                serde_json::from_value(detail["archived"].clone())
                                            {
                                                emit(CatalogEvent::Archived(scope.clone(), ids))?;
                                            }
                                        }
                                        _ => (),
                                    }
                                }
                            }
                            Server::Error { message, .. } => return Err(io::Error::other(message)),
                            _ => (),
                        }
                    }
                };
                let mut pause = Duration::from_millis(300);
                loop {
                    let started = std::time::Instant::now();
                    if let Err(error) = read()
                        && tx.send(CatalogEvent::Disconnected(error.kind())).is_err()
                    {
                        break;
                    }
                    connection.lock().unwrap().take();
                    if cancelled.recv_timeout(pause) != Err(mpsc::RecvTimeoutError::Timeout) {
                        break;
                    }
                    pause = if started.elapsed() > Duration::from_secs(15) {
                        Duration::from_millis(300)
                    } else {
                        (pause * 2).min(Duration::from_secs(if link.remote().is_some() {
                            30
                        } else {
                            5
                        }))
                    };
                }
            })?;
        Ok((CatalogWatch { socket, cancel }, rx))
    }
}
