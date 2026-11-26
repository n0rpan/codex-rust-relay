//! Paired host directory and signed operator settings.
mod keys;
use crate::{
    client::{self, Invitation, PairedClient},
    host_updates,
};
use ox_desk_core::{
    host_protocol::{Client, Server, VERSION},
    settings_protocol::{self as protocol, KeySelection, TransferredKey, model::Snapshot},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{self, BufReader},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

pub(super) fn directory() -> io::Result<PathBuf> {
    ox_desk_core::desktop_behavior::directory()
        .ok_or_else(|| io::Error::other("Desktop state directory unavailable"))
}
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedHost {
    // Used in local state filenames. Validate even when reading an older directory.
    #[serde(deserialize_with = "host_id")]
    pub id: String,
    pub name: String,
    pub origin: String,
    pub(crate) key_id: String,
    storage: keys::Storage,
    #[serde(skip)]
    background: bool,
    #[serde(skip)]
    #[cfg(feature = "server")]
    pub(crate) web_destination: bool,
    #[serde(skip)]
    client: Arc<OnceLock<PairedClient>>,
}
pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}
fn host_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let id = String::deserialize(d)?;
    if valid_id(&id) {
        Ok(id)
    } else {
        Err(serde::de::Error::custom("Invalid saved host ID"))
    }
}
pub fn list() -> io::Result<Vec<SavedHost>> {
    read_hosts(&directory()?.join("hosts.json"))
}
pub(crate) fn read_hosts(path: &Path) -> io::Result<Vec<SavedHost>> {
    match std::fs::File::open(path) {
        Ok(file) => Ok(serde_json::from_reader(file)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}
#[cfg(feature = "server")]
pub(crate) fn destinations(path: &Path) -> io::Result<Vec<SavedHost>> {
    let mut hosts = read_hosts(path)?;
    for host in &mut hosts {
        host.web_destination = true;
    }
    for desktop in list()? {
        if !hosts
            .iter()
            .any(|h| h.id == desktop.id || h.origin == desktop.origin)
        {
            hosts.push(desktop);
        }
    }
    for host in &mut hosts {
        host.background = true;
    }
    Ok(hosts)
}
#[cfg(feature = "server")]
pub(crate) fn worker_links(path: &Path) -> io::Result<Vec<SavedHost>> {
    let mut rows = read_hosts(path)?;
    for row in &mut rows {
        row.background = true;
    }
    Ok(rows)
}
impl SavedHost {
    pub fn same_pairing(&self, other: &Self) -> bool {
        self.id == other.id && self.key_id == other.key_id
    }
    pub fn client(&self) -> io::Result<&PairedClient> {
        if let Some(c) = self.client.get() {
            return Ok(c);
        }
        let c = PairedClient::new(
            &self.origin,
            &keys::load(&self.key_id, self.storage, self.background)?,
        )?;
        let _ = self.client.set(c);
        Ok(self.client.get().unwrap())
    }
    pub fn settings(&self, value: Value) -> io::Result<Value> {
        let c = self.client()?;
        client::runtime()?.block_on(c.settings(value))
    }
    pub fn updates(&self, command: host_updates::Command) -> io::Result<host_updates::Status> {
        Ok(serde_json::from_value(self.settings(
            json!({"action":"host_update","command":command}),
        )?)?)
    }
    pub fn snapshot(&self) -> Result<Snapshot, String> {
        self.settings(json!({"action":"read_rows"}))
            .and_then(|v| Ok(serde_json::from_value(v)?))
            .map_err(|e| e.to_string())
    }
    pub fn models(&self) -> Result<Vec<String>, String> {
        self.settings(json!({"action":"models"}))
            .and_then(|v| Ok(serde_json::from_value(v)?))
            .map_err(|e| e.to_string())
    }
    pub fn choose(
        &self,
        path: &std::path::Path,
        route: &ox_desk_core::store::Route,
    ) -> io::Result<PathBuf> {
        let reply = self.settings(json!({"action":"choose_project","path":path,"route":route}))?;
        serde_json::from_value(reply["project"].clone()).map_err(io::Error::other)
    }
    pub fn folders(&self, path: Option<PathBuf>, after: Option<String>) -> io::Result<Folders> {
        Ok(serde_json::from_value(self.settings(
            json!({"action":"browse_folders","path":path,"after":after}),
        )?)?)
    }
    pub fn revoke(&self) -> io::Result<()> {
        client::runtime()?.block_on(self.client()?.revoke_self())?;
        self.forget()
    }
    pub fn forget(&self) -> io::Result<()> {
        self.forget_from(&directory()?.join("hosts.json"))
    }
    pub(crate) fn forget_from(&self, registry: &Path) -> io::Result<()> {
        // Keep the reference available if the OS key store needs to be unlocked.
        keys::remove(&self.key_id, self.storage)?;
        ox_desk_core::state_file::update_preference_at(
            registry,
            Vec::new,
            |mut rows: Vec<SavedHost>| {
                rows.retain(|s| !s.same_pairing(self));
                rows
            },
        )
    }
    pub fn connect(&self) -> io::Result<std::os::unix::net::UnixStream> {
        let c = self.client()?;
        client::runtime()?.block_on(c.session())
    }
}
pub fn device_label() -> String {
    let mut bytes = [0_u8; 256];
    // POSIX gethostname can leave a truncated name unterminated.
    if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } == 0 {
        let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
        let label: String = String::from_utf8_lossy(&bytes[..end])
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();
        if !label.trim().is_empty() {
            return label;
        }
    }
    "OX Desktop".into()
}

pub fn pair(address: &str, code: &str, label: &str) -> io::Result<SavedHost> {
    enroll(
        Invitation::parse(address, code)?,
        label,
        &directory()?.join("hosts.json"),
        false,
        None,
        |_, _| true,
    )
}
pub(crate) fn enroll(
    invite: Invitation,
    label: &str,
    registry: &Path,
    background: bool,
    worker_source: Option<&str>,
    accept: impl FnOnce(&[SavedHost], &SavedHost) -> bool,
) -> io::Result<SavedHost> {
    let origin = invite.origin.clone();
    let rt = client::runtime()?;
    let credential = rt.block_on(client::register_as(invite, label, worker_source))?;
    let revoke = |client: &PairedClient| {
        if worker_source.is_some() {
            let _ = rt.block_on(client.worker_request(json!({"action":"revoke_self"})));
        } else {
            let _ = rt.block_on(client.revoke_self());
        }
    };
    let client = PairedClient::new(&origin, &credential).inspect_err(|_| {
        let _ = credential.discard_key();
    })?;
    let host = prepare_pairing(origin, &credential, &client, background, worker_source)
        .inspect_err(|_| {
            revoke(&client);
            let _ = credential.discard_key();
        })?;
    let mut old = Vec::new();
    let mut duplicate = false;
    let saved = ox_desk_core::state_file::update_preference_at(
        registry,
        Vec::new,
        |mut rows: Vec<SavedHost>| {
            if !accept(&rows, &host) {
                duplicate = true;
                return rows;
            }
            rows.retain(|r| {
                if r.id == host.id || r.origin == host.origin {
                    old.push(r.clone());
                    false
                } else {
                    true
                }
            });
            rows.push(host.clone());
            rows
        },
    );
    if saved.is_err() || duplicate {
        revoke(&client);
        let _ = keys::remove(&host.key_id, host.storage);
        saved?;
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "This host is already paired.",
        ));
    }
    for mut previous in old {
        previous.background = background;
        if let Ok(client) = previous.client() {
            revoke(client);
        }
        let _ = keys::remove(&previous.key_id, previous.storage);
    }
    Ok(host)
}
fn prepare_pairing(
    origin: String,
    credential: &client::Credential,
    client: &PairedClient,
    background: bool,
    worker_source: Option<&str>,
) -> io::Result<SavedHost> {
    let (id, name) = if let Some(source) = worker_source {
        let info = client::runtime()?.block_on(client.worker_request(json!({"action":"info"})))?;
        if info["grant"]["source"].as_str() != Some(source) {
            return Err(io::Error::other(
                "The receiving host did not bind this link to its source",
            ));
        }
        (
            info["id"].as_str().map(str::to_owned),
            info["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        )
    } else {
        operator_identity(client)?
    };
    let id = id
        .filter(|s| valid_id(s))
        .ok_or_else(|| io::Error::other("Update this OX host to report its identity."))?;
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let key_id = format!("{:032x}", u128::from_be_bytes(nonce));
    let storage = keys::save(&key_id, credential, background)?;
    Ok(SavedHost {
        id,
        name: name.unwrap_or_else(|| origin.clone()),
        origin,
        key_id,
        storage,
        background,
        #[cfg(feature = "server")]
        web_destination: background,
        client: Arc::new(OnceLock::from(client.clone())),
    })
}

fn operator_identity(client: &PairedClient) -> io::Result<(Option<String>, Option<String>)> {
    let mut socket = client::runtime()?.block_on(client.session())?;
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    ox_desk_core::host_protocol::write_frame(&mut socket, &Client::Hello { version: VERSION })?;
    let frame = ox_desk_core::host_protocol::read_frame(&mut BufReader::new(socket))?;
    let Server::Hello {
        host: Some(info), ..
    } = frame
    else {
        return Err(io::Error::other(
            "Update this OX host to report its identity.",
        ));
    };
    Ok((info.id, info.name))
}

#[derive(Deserialize)]
pub struct Folders {
    pub path: PathBuf,
    pub parent: Option<PathBuf>,
    pub folders: Vec<String>,
    pub more: bool,
}

#[derive(Serialize)]
pub struct ReviewRow {
    pub key: KeySelection,
    pub status: String,
    pub selected: bool,
    pub available: bool,
}
pub struct KeyReview {
    pub host: SavedHost,
    pub rows: Vec<ReviewRow>,
    pub(crate) source: String,
    pub(crate) target: String,
}
impl KeyReview {
    pub fn prepare(host: SavedHost, local: Snapshot) -> io::Result<Self> {
        let remote: Snapshot =
            serde_json::from_value(host.settings(json!({"action":"read_rows"}))?)?;
        let rows = local
            .credentials
            .into_iter()
            .filter(|r| r.saved || r.configured && r.environment)
            .map(|r| {
                let conflicting = remote
                    .credentials
                    .iter()
                    .any(|v| v.name == r.name && v.scope != r.scope && (v.configured || v.saved));
                let prior = remote
                    .credentials
                    .iter()
                    .find(|other| other.saved && other.name == r.name);
                let mut status = match prior {
                    Some(existing) => format!("Replace key for {}", existing.scope),
                    None => "Add key".into(),
                };
                let environment = remote
                    .credentials
                    .iter()
                    .any(|d| d.name == r.name && d.environment);
                if environment {
                    status.push_str(" · server environment takes precedence");
                }
                if !remote
                    .credentials
                    .iter()
                    .any(|d| d.name == r.name && d.scope == r.scope && d.configured)
                {
                    status.push_str(" · configure this endpoint on the host to use the key");
                }
                let selection = KeySelection {
                    name: r.name,
                    scope: r.scope,
                };
                if conflicting {
                    status = "This name belongs to another endpoint on the host".into();
                }
                ReviewRow {
                    key: selection,
                    status,
                    selected: !conflicting && prior.is_none() && !environment,
                    available: !conflicting,
                }
            })
            .collect();
        Ok(Self {
            host,
            rows,
            source: local.revisions.keys,
            target: remote.revisions.keys,
        })
    }
    pub fn send(
        self,
        export: impl FnOnce(protocol::Request) -> Result<Vec<TransferredKey>, String>,
    ) -> io::Result<usize> {
        let selected: Vec<_> = self
            .rows
            .into_iter()
            .filter(|row| row.available && row.selected)
            .map(|row| row.key)
            .collect();
        let count = selected.len();
        if count == 0 {
            return Err(io::Error::other("Select at least one API key."));
        }
        let keys: Vec<TransferredKey> = export(protocol::Request::ExportKeys {
            revision: self.source,
            selected,
        })
        .map_err(io::Error::other)?;
        let action = serde_json::to_value(protocol::Request::ImportKeys {
            revision: self.target,
            keys,
        })?;
        match serde_json::from_value(self.host.settings(action)?)? {
            protocol::model::Outcome::Applied => Ok(count),
            protocol::model::Outcome::ChangedOnDisk { .. } => Err(io::Error::other(
                "The server’s keys changed. Review the transfer again.",
            )),
        }
    }
}
