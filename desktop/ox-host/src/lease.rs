//! A private per-boot lease. A kernel close on host death also reaches children
//! supervised by an OX helper, including workers in their own process groups.
use crate::{engine::Message, transport::same_user};
use serde::Deserialize;
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender},
    thread,
    time::Duration,
};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Lease {
        id: String,
        parent: Option<String>,
    },
    Worker {
        scope: ox_desk_core::host_protocol::Scope,
        parent: Option<String>,
        write: bool,
        claude: bool,
    },
}

pub(crate) struct Endpoint {
    pub path: PathBuf,
    listener: UnixListener,
}
impl Endpoint {
    pub(crate) fn bind(socket: &Path) -> io::Result<Self> {
        let dir = socket
            .parent()
            .ok_or_else(|| io::Error::other("Invalid socket directory"))?;
        // Only discard socket inodes left by a previous incarnation for this same host.
        let stem = socket
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| io::Error::other("Invalid socket name"))?;
        let prefix = format!(".{stem}-lease-");
        for ent in fs::read_dir(dir)? {
            let ent = ent?;
            let name = ent.file_name();
            if name
                .to_str()
                .and_then(|s| s.strip_prefix(&prefix))
                .is_some_and(|s| s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                let m = ent.path().symlink_metadata()?;
                if m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() } {
                    fs::remove_file(ent.path())?;
                }
            }
        }
        let id = getrandom::u32().map_err(io::Error::other)?;
        let path = dir.join(format!("{prefix}{id:08x}"));
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Self { path, listener })
    }
    pub(crate) fn listen(&self, tx: SyncSender<Message>) -> io::Result<()> {
        let listener = self.listener.try_clone()?;
        let leases = std::sync::Arc::new(std::sync::Mutex::new(Groups::default()));
        thread::spawn(move || {
            for socket in listener.incoming().flatten() {
                let tx = tx.clone();
                let leases = leases.clone();
                thread::spawn(move || {
                    let _ = serve(socket, tx, &leases);
                });
            }
        });
        Ok(())
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
#[derive(Default)]
struct Groups(std::collections::HashMap<String, (Option<String>, UnixStream)>);
impl Groups {
    fn register(
        &mut self,
        id: &str,
        parent: Option<String>,
        socket: &UnixStream,
    ) -> io::Result<()> {
        if id.is_empty()
            || id.len() > 128
            || self.0.contains_key(id)
            || parent.as_ref().is_some_and(|p| !self.0.contains_key(p))
        {
            return Err(io::Error::other("The launching OX process has closed"));
        }
        self.0.insert(id.to_owned(), (parent, socket.try_clone()?));
        Ok(())
    }
    fn retire(&mut self, id: &str) {
        let mut remove = vec![id.to_owned()];
        while let Some(id) = remove.pop() {
            if let Some((_, stream)) = self.0.remove(&id) {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                remove.extend(
                    self.0
                        .iter()
                        .filter(|(_, (parent, _))| parent.as_ref() == Some(&id))
                        .map(|(k, _)| k.clone()),
                );
            }
        }
    }
}
fn serve(
    mut socket: UnixStream,
    tx: SyncSender<Message>,
    groups: &std::sync::Mutex<Groups>,
) -> io::Result<()> {
    same_user(&socket)?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut size_buf = [0; 4];
    socket.read_exact(&mut size_buf)?;
    let size = u32::from_be_bytes(size_buf) as usize;
    if size > 16384 {
        return Err(io::Error::other("Worker lease frame too large"));
    }
    let mut frame = vec![0; size];
    socket.read_exact(&mut frame)?;
    match serde_json::from_slice(&frame)? {
        Request::Lease { id, parent } => {
            groups.lock().unwrap().register(&id, parent, &socket)?;
            let result = (|| {
                socket.write_all(&[1])?;
                socket.set_read_timeout(None)?;
                socket.read(&mut [0]).map(|_| ())
            })();
            groups.lock().unwrap().retire(&id);
            result?;
        }
        Request::Worker {
            scope,
            parent,
            write,
            claude,
        } => {
            let (reply, result) = mpsc::sync_channel(1);
            tx.send(Message::Worker {
                scope,
                parent,
                write,
                claude,
                reply,
            })
            .map_err(io::Error::other)?;
            let Some((permit, parent)) = result.recv_timeout(Duration::from_secs(4)).ok().flatten()
            else {
                socket.write_all(&[0])?;
                return Ok(());
            };
            let registered = (|| -> io::Result<bool> {
                socket.write_all(&[1])?;
                socket.write_all(&(parent.len() as u32).to_be_bytes())?;
                socket.write_all(parent.as_bytes())?;
                socket.read_exact(&mut size_buf)?;
                let n = u32::from_be_bytes(size_buf) as usize;
                if n > 64 * 1024 {
                    return Err(io::Error::other("Worker registration too large"));
                }
                let mut bytes = vec![0; n];
                socket.read_exact(&mut bytes)?;
                let registration = serde_json::from_slice(&bytes)?;
                let (reply, result) = mpsc::sync_channel(1);
                tx.send(Message::RegisterJob {
                    permit,
                    registration,
                    reply,
                })
                .map_err(io::Error::other)?;
                Ok(result.recv_timeout(Duration::from_secs(4)).unwrap_or(false))
            })();
            let _ = tx.send(Message::Release(permit));
            socket.write_all(&[u8::from(registered.unwrap_or(false))])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loss_of_a_helper_retires_only_its_descendants() {
        let (a, _) = UnixStream::pair().unwrap();
        let (b, mut child) = UnixStream::pair().unwrap();
        let mut groups = Groups::default();
        groups.register("root", None, &a).unwrap();
        groups.register("child", Some("root".into()), &b).unwrap();
        groups
            .register("grandchild", Some("child".into()), &a)
            .unwrap();
        groups.register("other", None, &a).unwrap();
        groups.retire("root");
        assert_eq!(child.read(&mut [0]).unwrap(), 0);
        assert_eq!(groups.0.len(), 1);
        assert!(groups.0.contains_key("other"));
        assert!(groups.register("late", Some("child".into()), &a).is_err());
    }
}
