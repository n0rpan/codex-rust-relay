//! Read the installed Codex command schema away from the window. The executable's
//! identity keys the cache so an upgrade can be discovered in a new connection.
use crate::slash::Capabilities;
use std::{
    fs, io,
    os::unix::{fs::MetadataExt, process::CommandExt},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

type Identity = (PathBuf, u64, u64, i64, i64);
#[derive(Default)]
pub struct SchemaReader {
    cache: Mutex<Option<(Identity, Capabilities)>>,
    stopped: AtomicBool,
}

pub fn installed(path: &std::ffi::OsStr) -> io::Result<Capabilities> {
    static READER: LazyLock<SchemaReader> = LazyLock::new(SchemaReader::default);
    READER.read(path)
}

impl SchemaReader {
    /// Stop accepting requests and wait until the active generator is reaped.
    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        drop(self.cache.lock().unwrap_or_else(|err| err.into_inner()));
    }

    pub fn read(&self, path: &std::ffi::OsStr) -> io::Result<Capabilities> {
        let bin = std::env::split_paths(path)
            .filter(|p| p.is_absolute())
            .map(|p| p.join("codex"))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Install Codex to browse its commands",
                )
            })?;
        let meta = fs::metadata(&bin)?;
        let key = (
            bin.clone(),
            meta.dev(),
            meta.ino(),
            meta.mtime(),
            meta.mtime_nsec(),
        );
        let mut cache = self.cache.lock().unwrap();
        if self.stopped.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Codex command discovery stopped",
            ));
        }
        if let Some((cached, caps)) = &*cache
            && cached == &key
        {
            return Ok(caps.clone());
        }
        let dir = tempfile::tempdir()?;
        let mut child = Command::new(bin)
            .env("PATH", path)
            .args([
                "app-server",
                "generate-json-schema",
                "--experimental",
                "--out",
            ])
            .arg(dir.path())
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(io::Error::other(
                        "Codex could not generate its command schema",
                    ));
                }
                break;
            }
            let stopping = self.stopped.load(Ordering::Acquire);
            if stopping || start.elapsed() > Duration::from_secs(5) {
                // The executable may be a launcher with children. Its unreaped pid
                // still identifies the process group we created for this lookup.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                child.wait()?;
                return if stopping {
                    Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "Codex command discovery stopped",
                    ))
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Codex command discovery timed out",
                    ))
                };
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        let mut caps = Capabilities::default();
        caps.read_schema(&serde_json::from_slice(&fs::read(
            dir.path().join("ClientRequest.json"),
        )?)?);
        *cache = Some((key, caps.clone()));
        Ok(caps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Read,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        sync::Arc,
    };

    #[test]
    fn shutting_down_reaps_a_slow_schema_launcher_and_its_child() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let listener = UnixListener::bind(dir.path().join("ready")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let bin = dir.path().join("codex");
        fs::write(
            &bin,
            r#"#!/usr/bin/env python3
import pathlib, socket, subprocess
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(str(pathlib.Path(__file__).with_name("ready")))
with subprocess.Popen(["sleep", "30"], pass_fds=(sock.fileno(),)) as child:
    sock.sendall(b"x")
    child.wait()
"#,
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(dir.path().to_owned())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let reader = Arc::new(SchemaReader::default());
        let job = {
            let reader = reader.clone();
            std::thread::spawn(move || reader.read(&path))
        };
        let until = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock && Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(err) => panic!("schema fixture did not start: {err}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket.read_exact(&mut [0]).unwrap();
        reader.shutdown();
        assert_eq!(
            job.join().unwrap().err().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
        // Both processes held this connection. EOF proves that neither survived shutdown.
        assert_eq!(socket.read(&mut [0]).unwrap(), 0);
    }
}
