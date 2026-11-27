//! Per-user host discovery and lifecycle shared by the daemon and local desktop.
pub mod checkout;
pub mod errlog;
#[path = "../../../src/managed_install.rs"]
pub mod managed_install;
mod manager;
pub use manager::{manage, refresh_update_service};

/// Whether the login service (launchd agent or systemd user unit) is installed.
pub fn service_installed() -> io::Result<bool> {
    manager::installed()
}
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

type FileIdentity = (u64, u64, u64, i64, i64, i64, i64);
pub type RouteInputs = [Option<FileIdentity>; 4];

/// File identities behind the settings snapshot's provider aliases and favorites.
/// An unreadable input produces no stamp so callers bypass their cache.
pub fn route_inputs() -> Option<RouteInputs> {
    let home = env::var_os("CODEX_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".codex")))?;
    let mut ids = [None; 4];
    for (i, name) in [
        "codeox.toml",
        "config.toml",
        "ox-api-keys.json",
        "ox-favorites.json",
    ]
    .into_iter()
    .enumerate()
    {
        match fs::metadata(home.join(name)) {
            Ok(m) => {
                ids[i] = Some((
                    m.dev(),
                    m.ino(),
                    m.len(),
                    m.mtime(),
                    m.mtime_nsec(),
                    m.ctime(),
                    m.ctime_nsec(),
                ))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(_) => return None,
        }
    }
    Some(ids)
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

/// The start of the error for an OX state folder that other accounts could read. The
/// desktop recognises it to offer a one-click fix, so keep it stable.
pub const UNSAFE_FOLDER: &str = "OX state folder has unsafe permissions";

fn private_directory(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let m = fs::symlink_metadata(path)?;
    if m.is_dir() && m.uid() == uid() && m.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{UNSAFE_FOLDER}: {}", path.display()),
        ));
    }
    if !m.is_dir() || m.uid() != uid() {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "OX can't use {} because it is not a folder that only you own. Move or delete it, then reconnect.",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// The folder named by an [`UNSAFE_FOLDER`] error message, if `message` is one.
pub fn unsafe_folder(message: &str) -> Option<PathBuf> {
    let path = message.strip_prefix(UNSAFE_FOLDER)?.strip_prefix(": ")?;
    let path = PathBuf::from(path.lines().next()?);
    path.is_absolute().then_some(path)
}

/// The shell command that fixes an unsafe folder by hand.
pub fn fix_command(path: &Path) -> String {
    format!(
        "chmod 700 '{}'",
        path.display().to_string().replace('\'', "'\\''")
    )
}

/// Make a folder OX created private again (mode 700). Only a real folder owned by the
/// current account is changed; anything else is refused.
pub fn fix_folder_permissions(path: &Path) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != uid() {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{} is not a folder you own", path.display()),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
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
/// Ownership of a state or socket lock. Explicit release prevents an unrelated
/// child's short fork-to-exec window from extending host ownership.
pub struct HostLock(Option<File>);
impl HostLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let file = lock_file(path)?;
        file.try_lock().map_err(io::Error::from)?;
        Ok(Self(Some(file)))
    }

    /// Transfer lock ownership to the update installer through its stdin.
    pub fn into_file(mut self) -> File {
        self.0.take().expect("lock is owned")
    }
}
impl Drop for HostLock {
    fn drop(&mut self) {
        if let Some(f) = &self.0 {
            let _ = f.unlock();
        }
    }
}
/// Keep an absent host from binding while Desktop selects a package. If another
/// process owns the lock it is already running, starting, or shutting down.
pub fn reserve_stopped_host(paths: &Paths) -> io::Result<Option<HostLock>> {
    paths.prepare()?;
    match HostLock::acquire(&paths.state_dir.join("owner.lock")) {
        Ok(lock) => Ok(Some(lock)),
        Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

pub struct SocketGuard {
    pub listener: UnixListener,
    socket: PathBuf,
    identity: (u64, u64),
    _state_lock: HostLock,
    _socket_lock: HostLock,
}
impl SocketGuard {
    pub fn bind(paths: &Paths) -> io::Result<Self> {
        paths.prepare()?;
        let state_lock = HostLock::acquire(&paths.state_dir.join("owner.lock"))?;
        let socket_lock = HostLock::acquire(&paths.socket.with_extension("owner.lock"))?;
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
    let executable_path = env::current_exe()?;
    if let Some(store) = managed_install::package_store(&executable_path) {
        return Ok(managed_install::active_binary(&store, "ox-host"));
    }
    let sibling = executable_path.with_file_name("ox-host");
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
    fn idle_update_reserves_a_stopped_host_without_starting_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            state_dir: dir.path().join("state"),
            socket: dir.path().join("runtime/host.sock"),
        };
        let guard = reserve_stopped_host(&paths).unwrap().unwrap();
        assert!(!is_running(&paths).unwrap());
        assert!(SocketGuard::bind(&paths).is_err());
        drop(guard);
        let host = SocketGuard::bind(&paths).unwrap();
        assert!(reserve_stopped_host(&paths).unwrap().is_none());
        drop(host);
        assert!(reserve_stopped_host(&paths).unwrap().is_some());
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
