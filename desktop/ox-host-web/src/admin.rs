use serde::{Deserialize, Serialize};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

pub const SOCKET: &str = "remote-admin.sock";
/// Both Darwin and Linux limit sockaddr_un. Isolate long state paths by owner and a stable hash.
pub fn socket_path(state_dir: &Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let path = state_dir.join(SOCKET);
    if path.as_os_str().as_bytes().len() < 100 {
        return path;
    }
    let hash = state_dir
        .as_os_str()
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    Path::new("/tmp")
        .join(format!("ox-remote-{}-{hash:016x}", unsafe {
            libc::geteuid()
        }))
        .join(SOCKET)
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    HostProfile {
        edit: Option<crate::host_profile::Edit>,
    },
    Update {
        command: crate::host_updates::Command,
    },
    Status,
    Enable {
        origin: String,
        port: u16,
    },
    Reserve {
        origin: String,
        port: u16,
    },
    ReserveRemoval {
        origin: String,
        expected_enabled: bool,
    },
    Commit {
        ticket: String,
    },
    Disable,
    Pair {
        login: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        terminal: bool,
    },
    WorkerLinks {
        command: WorkerLinkAction,
    },
    #[cfg(feature = "client")]
    WorkerAccess {
        id: String,
        before: WorkerGrant,
        may_launch: bool,
        ceiling: ox_desk_core::modes::Level,
    },
    WorkerInvitation {
        login: String,
    },
    Revoke {
        id: String,
    },
    TerminalGrant {
        id: String,
        enabled: bool,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceClass {
    #[default]
    Phone,
    Desktop,
    Host,
}

#[cfg(feature = "client")]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerGrant {
    /// Stable ID of the OX host that redeemed this invitation.
    pub source: String,
    pub may_launch: bool,
    pub ceiling: ox_desk_core::modes::Level,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    #[serde(default)]
    pub class: DeviceClass,
    #[cfg(feature = "client")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerGrant>,
    #[serde(default)]
    pub terminal: bool,
    pub id: String,
    pub label: String,
    pub login: String,
    pub last_used: i64,
}

#[derive(Serialize, Deserialize)]
pub struct Pairing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_code: Option<String>,
    pub url: String,
    pub qr_svg: String,
    pub expires_at: i64,
}

#[derive(Serialize, Deserialize)]
pub struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<crate::host_profile::HostProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<crate::host_updates::Status>,
    pub enabled: bool,
    pub origin: Option<String>,
    pub port: u16,
    pub devices: Vec<Device>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worker_hosts: Vec<Device>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_links: Option<serde_json::Value>,
    pub pairing: Option<Pairing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerLinkAction {
    WorkerLinks,
    AddWorkerLink { address: String, code: String },
    LinkPairedHost { host: String },
    RemoveWorkerLink { host: String },
}
// Listener configuration and terminal grants have a private, same-user administration socket.
pub fn request(state_dir: &Path, req: &Request) -> io::Result<Reply> {
    exchange(state_dir, req).map(|(_, reply)| reply)
}

pub struct Reservation {
    state: PathBuf,
    ticket: String,
    _lease: UnixStream,
}

impl Reservation {
    pub fn bind(state_dir: &Path, origin: String, port: u16) -> io::Result<Self> {
        Self::open(state_dir, Request::Reserve { origin, port })
    }
    pub fn removal(state_dir: &Path, origin: String, expected_enabled: bool) -> io::Result<Self> {
        Self::open(
            state_dir,
            Request::ReserveRemoval {
                origin,
                expected_enabled,
            },
        )
    }
    fn open(state_dir: &Path, request: Request) -> io::Result<Self> {
        let (lease, reply) = exchange(state_dir, &request)?;
        let ticket = reply
            .reservation
            .ok_or_else(|| io::Error::other("Host did not reserve the Serve port"))?;
        Ok(Self {
            state: state_dir.into(),
            ticket,
            _lease: lease,
        })
    }
    pub fn commit(self) -> io::Result<Reply> {
        request(
            &self.state,
            &Request::Commit {
                ticket: self.ticket,
            },
        )
    }
}

fn exchange(state_dir: &Path, req: &Request) -> io::Result<(UnixStream, Reply)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let path = socket_path(state_dir);
    let dir = std::fs::symlink_metadata(path.parent().expect("socket directory"))?;
    let sock = std::fs::symlink_metadata(&path)?;
    let uid = unsafe { libc::geteuid() };
    if !dir.is_dir()
        || dir.uid() != uid
        || dir.mode() & 0o077 != 0
        || !sock.file_type().is_socket()
        || sock.uid() != uid
        || sock.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Invalid remote administration socket permissions",
        ));
    }
    let mut stream = UnixStream::connect(path)?;
    // Pairing can include several bounded HTTPS exchanges on the remote host.
    stream.set_read_timeout(Some(Duration::from_secs(
        if matches!(req, Request::WorkerLinks { .. }) {
            75
        } else {
            8
        },
    )))?;
    stream.set_write_timeout(Some(Duration::from_secs(4)))?;
    serde_json::to_writer(&mut stream, req)?;
    stream.write_all(b"\n")?;
    let mut line = Vec::new();
    BufReader::new((&mut stream).take(262145)).read_until(b'\n', &mut line)?;
    if line.len() > 262144 || !line.ends_with(b"\n") {
        return Err(io::Error::other("Invalid remote administration reply"));
    }
    let reply: Result<Reply, String> = serde_json::from_slice(&line)?;
    Ok((stream, reply.map_err(io::Error::other)?))
}

/// Accept the usual Crockford base32 substitutions when typing an invitation.
pub fn manual_code(input: &str) -> Option<String> {
    let text: String = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'-')
        .map(|b| match b.to_ascii_uppercase() {
            b'O' => '0',
            b'I' | b'L' => '1',
            b => char::from(b),
        })
        .collect();
    (text.len() == 12 && text.bytes().all(|b| b.is_ascii_alphanumeric() && b != b'U'))
        .then_some(text)
}
