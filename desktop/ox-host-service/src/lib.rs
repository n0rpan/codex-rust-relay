//! Per-user host discovery and lifecycle shared by the daemon and local desktop.
mod manager;
pub use manager::manage;
use std::{
    env,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, ErrorKind},
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Narrow glibc arena growth on machines where touching a new arena can fault
/// an entire transparent huge page. Call before starting host or GUI threads.
pub fn configure_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if env::var_os("MALLOC_ARENA_MAX").is_none()
        && !env::var("GLIBC_TUNABLES")
            .unwrap_or_default()
            .split(':')
            .any(|v| v.starts_with("glibc.malloc.arena_max="))
    {
        // SAFETY: mallopt takes two integers and configures only this process.
        unsafe { libc::mallopt(libc::M_ARENA_MAX, 2) };
    }
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub socket: PathBuf,
    pub state_dir: PathBuf,
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}
fn home() -> io::Result<PathBuf> {
    absolute_env("HOME").ok_or_else(|| io::Error::other("Set HOME to an absolute directory."))
}
fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

impl Paths {
    pub fn discover() -> io::Result<Self> {
        let state_dir = if let Some(root) = absolute_env("OX_HOST_STATE_DIR") {
            root
        } else {
            absolute_env("XDG_STATE_HOME")
                .unwrap_or(home()?.join(if cfg!(target_os = "macos") {
                    "Library/Application Support"
                } else {
                    ".local/state"
                }))
                .join("ox/host")
        };
        let socket = if let Some(path) = absolute_env("OX_HOST_SOCKET") {
            path
        } else {
            // Scope fixtures and separate state homes independently, even when their login
            // shares a runtime directory. sockaddr_un has a short fixed path on both OSes.
            let hash = state_dir
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .fold(0xcbf29ce484222325u64, |v, b| {
                    (v ^ u64::from(*b)).wrapping_mul(0x100000001b3)
                });
            let name = format!("ox-host-{}-{hash:016x}", uid());
            absolute_env("XDG_RUNTIME_DIR")
                .map(|p| p.join(&name).join("host.sock"))
                .filter(|p| p.as_os_str().as_encoded_bytes().len() < 100)
                .unwrap_or_else(|| Path::new("/tmp").join(name).join("host.sock"))
        };
        Ok(Self { socket, state_dir })
    }
    fn prepare(&self) -> io::Result<()> {
        if !self.socket.is_absolute() || !self.state_dir.is_absolute() {
            return Err(io::Error::other("Host paths must be absolute."));
        }
        private_directory(&self.state_dir)?;
        private_directory(
            self.socket
                .parent()
                .ok_or_else(|| io::Error::other("Missing socket directory"))?,
        )
    }
}

fn private_directory(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != uid() || m.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "{} must be a private directory owned by the current account",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn lock_file(path: &Path) -> io::Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = f.metadata()?;
    if !m.is_file() || m.uid() != uid() || m.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "Invalid host lock file",
        ));
    }
    Ok(f)
}
fn socket_meta(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() && m.uid() == uid() => Ok(Some(m)),
        Ok(_) => Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "Refusing to replace a non-host socket at {}",
                path.display()
            ),
        )),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
pub struct SocketGuard {
    pub listener: UnixListener,
    socket: PathBuf,
    identity: (u64, u64),
    _state_lock: File,
    _socket_lock: File,
}
impl SocketGuard {
    pub fn bind(paths: &Paths) -> io::Result<Self> {
        paths.prepare()?;
        let state_lock = lock_file(&paths.state_dir.join("owner.lock"))?;
        state_lock.try_lock().map_err(io::Error::from)?;
        let socket_lock = lock_file(&paths.socket.with_extension("owner.lock"))?;
        socket_lock.try_lock().map_err(io::Error::from)?;
        if socket_meta(&paths.socket)?.is_some() {
            match UnixStream::connect(&paths.socket) {
                Ok(_) => {
                    return Err(io::Error::new(
                        ErrorKind::AddrInUse,
                        "An OX host is already listening.",
                    ));
                }
                Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
                    fs::remove_file(&paths.socket)?
                }
                Err(e) if e.kind() == ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
        }
        let listener = UnixListener::bind(&paths.socket)?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
        let m = fs::symlink_metadata(&paths.socket)?;
        Ok(Self {
            listener,
            socket: paths.socket.clone(),
            identity: (m.dev(), m.ino()),
            _state_lock: state_lock,
            _socket_lock: socket_lock,
        })
    }
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.socket).is_ok_and(|m| (m.dev(), m.ino()) == self.identity) {
            let _ = fs::remove_file(&self.socket);
        }
    }
}

pub fn is_running(paths: &Paths) -> io::Result<bool> {
    if socket_meta(&paths.socket)?.is_none() {
        return Ok(false);
    }
    match UnixStream::connect(&paths.socket) {
        Ok(_) => Ok(true),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}
fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
}
fn binary() -> io::Result<PathBuf> {
    let sibling = env::current_exe()?.with_file_name("ox-host");
    if executable(&sibling) {
        return Ok(sibling);
    }
    env::var_os("PATH")
        .and_then(|path| {
            env::split_paths(&path)
                .filter(|p| p.is_absolute())
                .map(|p| p.join("ox-host"))
                .find(|p| executable(p))
        })
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                "Install the OX Desktop package containing ox-host.",
            )
        })
}
fn start_detached(paths: &Paths) -> io::Result<()> {
    let mut command = Command::new(binary()?);
    command
        .arg("run")
        .arg("--socket")
        .arg(&paths.socket)
        .arg("--state-dir")
        .arg(&paths.state_dir)
        .current_dir(home()?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Separate the terminal session as well as stdio from the desktop or shell.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Call from a background worker. Simultaneous clients launch at most one host.
pub fn ensure_running(paths: &Paths) -> io::Result<()> {
    paths.prepare()?;
    if is_running(paths)? {
        return Ok(());
    }
    let lock = lock_file(&paths.state_dir.join("startup.lock"))?;
    lock.lock()?;
    if is_running(paths)? {
        return Ok(());
    }
    if manager::installed()? {
        manager::start()?;
    } else {
        start_detached(paths)?;
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if is_running(paths)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                format!(
                    "OX host did not open {}. Run ox host status.",
                    paths.socket.display()
                ),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(root: &Path) -> Paths {
        Paths {
            state_dir: root.join("state"),
            socket: root.join("runtime/host.sock"),
        }
    }
    #[test]
    fn only_one_host_can_own_a_state_directory_or_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let a = fixture(tmp.path());
        let first = SocketGuard::bind(&a).unwrap();
        assert!(is_running(&a).unwrap());
        assert_eq!(
            fs::symlink_metadata(&a.socket).unwrap().mode() & 0o777,
            0o600
        );
        let same_state = Paths {
            state_dir: a.state_dir.clone(),
            socket: tmp.path().join("another/host.sock"),
        };
        let same_socket = Paths {
            state_dir: tmp.path().join("another-state"),
            socket: a.socket.clone(),
        };
        assert!(SocketGuard::bind(&same_state).is_err());
        assert!(SocketGuard::bind(&same_socket).is_err());
        drop(first);
        assert!(!is_running(&a).unwrap());
        let guard = SocketGuard::bind(&a).unwrap();
        fs::rename(&a.socket, a.socket.with_extension("old")).unwrap();
        let new = UnixListener::bind(&a.socket).unwrap();
        drop(guard);
        assert!(a.socket.exists());
        drop(new);
    }
    #[test]
    fn stale_socket_recovers_while_unrelated_files_and_symlinks_survive() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = fixture(tmp.path());
        paths.prepare().unwrap();
        drop(UnixListener::bind(&paths.socket).unwrap());
        drop(SocketGuard::bind(&paths).unwrap());
        fs::write(&paths.socket, "keep").unwrap();
        assert!(SocketGuard::bind(&paths).is_err());
        assert_eq!(fs::read_to_string(&paths.socket).unwrap(), "keep");
        fs::remove_file(&paths.socket).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("missing"), &paths.socket).unwrap();
        assert!(SocketGuard::bind(&paths).is_err());
        assert!(paths.socket.is_symlink());
    }
}
