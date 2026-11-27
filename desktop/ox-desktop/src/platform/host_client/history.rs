//! A desktop can broker history only for a transfer it initiated with these pairings.
use super::*;
use ox_desk_core::handoff::HandoffTransfer;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq)]
struct Pairing {
    host: String,
    device: Option<String>,
}
impl Pairing {
    fn matches(&self, link: &HostLink) -> bool {
        match (link.remote(), self.device.as_deref()) {
            (Some(h), Some(device)) => h.id == self.host && h.pairing_id() == device,
            (None, None) => true,
            _ => false,
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    sender: Pairing,
    recipient: Pairing,
    scope: Scope,
}
pub(crate) fn clean_handoff_pairings() {
    use sha2::{Digest, Sha256};
    let Some(dir) = ox_desk_core::desktop_behavior::directory() else {
        return;
    };
    let Ok(current) = super::super::hosts::list() else {
        return;
    };
    let live = |pair: &Pairing| {
        pair.device.as_ref().is_none_or(|id| {
            current
                .iter()
                .any(|h| h.pairing_id() == id && h.id == pair.host)
        })
    };
    if let Ok(entries) = dir.join("handoff-sources").read_dir() {
        for entry in entries.flatten() {
            let Ok(receipt) = std::fs::File::open(entry.path())
                .and_then(|f| serde_json::from_reader::<_, Receipt>(f).map_err(io::Error::from))
            else {
                continue;
            };
            if !live(&receipt.sender) || !live(&receipt.recipient) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    if let Ok(entries) = dir.join("handoff-brokers").read_dir() {
        let active: HashSet<_> = current
            .iter()
            .map(|h| h.pairing_id())
            .chain(std::iter::once("local"))
            .map(|k| format!("{:x}", Sha256::digest(k.as_bytes())))
            .collect();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && name.len() == 64
                && !active.contains(name)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}
/// One secret per target pairing. A second paired desktop cannot answer for the initiator.
pub(super) fn broker_key(link: &HostLink) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let root = ox_desk_core::desktop_behavior::directory()
        .ok_or_else(|| io::Error::other("Desktop state directory unavailable."))?;
    let dir = root.join("handoff-brokers");
    let pair = link.remote().map_or("local", |h| h.pairing_id());
    let file = dir.join(format!("{:x}", Sha256::digest(pair.as_bytes())));
    if let Ok(key) = std::fs::read_to_string(&file) {
        return Ok(key);
    }
    std::fs::create_dir_all(&dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let key: String = random.iter().map(|b| format!("{b:02x}")).collect();
    temp.write_all(key.as_bytes())?;
    match temp.persist_noclobber(&file) {
        Ok(_) => Ok(key),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => std::fs::read_to_string(file),
        Err(e) => Err(e.error),
    }
}
pub(super) fn binding(link: &HostLink) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "{:x}",
        Sha256::digest(broker_key(link)?.as_bytes())
    ))
}
fn path(id: &str) -> io::Result<PathBuf> {
    let id = uuid::Uuid::parse_str(id).map_err(io::Error::other)?;
    let root = ox_desk_core::desktop_behavior::directory()
        .ok_or_else(|| io::Error::other("Desktop state directory unavailable."))?;
    Ok(root.join("handoff-sources").join(format!("{id}.json")))
}
pub(super) fn record(
    from: &HostLink,
    to: &HostLink,
    packet: &HandoffTransfer,
) -> io::Result<PathBuf> {
    let src = packet
        .source
        .host
        .as_ref()
        .ok_or_else(|| io::Error::other("Missing source host."))?;
    if from.remote().is_some_and(|h| h.id != src.id) {
        return Err(io::Error::other("The source host identity changed."));
    }
    let receipt = Receipt {
        sender: Pairing {
            host: src.id.clone(),
            device: from.remote().map(|h| h.pairing_id().to_owned()),
        },
        recipient: Pairing {
            host: packet.target_host.clone(),
            device: to.remote().map(|h| h.pairing_id().to_owned()),
        },
        scope: packet.source.scope.clone(),
    };
    let file = path(&packet.id)?;
    match std::fs::File::open(&file) {
        Ok(old) => {
            let old: Receipt = serde_json::from_reader(old)?;
            if old.sender != receipt.sender
                || old.recipient != receipt.recipient
                || old.scope != receipt.scope
            {
                return Err(io::Error::other(
                    "This handoff was started with another pairing or conversation.",
                ));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            ox_desk_core::state_file::write_json(&file, &receipt)?
        }
        Err(e) => return Err(e),
    }
    Ok(file)
}
struct Work {
    writer: Arc<Mutex<UnixStream>>,
    peer: HostLink,
    token: String,
    target: String,
    transfer: String,
    page: ox_desk_core::host_protocol::HandoffPage,
}
impl Work {
    fn reply(self, result: Result<serde_json::Value, String>) -> io::Result<()> {
        write_frame(
            &mut *self.writer.lock().unwrap(),
            &Client::FinishHandoffRead {
                token: self.token,
                result,
            },
        )
    }
}
pub(super) fn relay(
    writer: Arc<Mutex<UnixStream>>,
    peer: HostLink,
    token: String,
    target: String,
    transfer: String,
    page: ox_desk_core::host_protocol::HandoffPage,
) -> io::Result<()> {
    // Leave catalog navigation responsive while several native sessions read at once.
    static QUEUE: std::sync::LazyLock<flume::Sender<Work>> = std::sync::LazyLock::new(|| {
        let (queue, rx) = flume::bounded::<Work>(32);
        for _ in 0..4 {
            let rx = rx.clone();
            thread::spawn(move || {
                while let Ok(job) = rx.recv() {
                    let result = read(&job.peer, &job.target, &job.transfer, job.page.clone())
                        .map_err(|e| {
                            if e.kind() == io::ErrorKind::NotFound {
                                String::new()
                            } else {
                                e.to_string()
                            }
                        });
                    let _ = job.reply(result);
                }
            });
        }
        queue
    });
    if let Err(err) = QUEUE.try_send(Work {
        writer,
        peer,
        token,
        target,
        transfer,
        page,
    }) {
        return err.into_inner().reply(Err(
            "Other conversation reads are still loading. Retry shortly.".into(),
        ));
    }
    Ok(())
}
pub(super) fn read(
    recipient: &HostLink,
    target: &str,
    transfer: &str,
    page: ox_desk_core::host_protocol::HandoffPage,
) -> io::Result<serde_json::Value> {
    let mut recipient = recipient.clone();
    let mut transfer = transfer.to_owned();
    let mut target = target.to_owned();
    let mut page = page;
    for _ in 0..32 {
        let file = std::fs::File::open(path(&transfer)?)?;
        let saved: Receipt = serde_json::from_reader(file)?;
        if saved.recipient.host != target || !saved.recipient.matches(&recipient) {
            return Err(io::Error::other("The receiving host pairing has changed."));
        }
        let link = match saved.sender.device.as_ref() {
            Some(_) => super::super::hosts::list()?
                .into_iter()
                .map(HostLink::Remote)
                .find(|h| saved.sender.matches(h))
                .ok_or_else(|| {
                    io::Error::other(
                        "The original source host is no longer paired with this desktop.",
                    )
                })?,
            None => HostLink::Local,
        };
        let name = link.remote().map_or("this computer", |h| h.name.as_str());
        let mut connection = link
            .handoff_connection()
            .map_err(|e| io::Error::other(format!("Could not reach {name}: {e}")))?;
        if !connection.2.iter().any(|c| c == "handoff_history") {
            return Err(io::Error::other(format!(
                "Update {name} to read the original conversation."
            )));
        }
        let result = handoff_call(
            &mut connection,
            Client::ReadHandoffPage {
                id: "history".into(),
                broker: Some(broker_key(&link)?),
                source_host: saved.sender.host.clone(),
                target_host: target.clone(),
                scope: saved.scope,
                transfer,
                page: page.clone(),
            },
            "history",
            || {},
        )?;
        let Some(next) = result["next_transfer"].as_str() else {
            return Ok(result);
        };
        transfer = next.to_owned();
        target = saved.sender.host;
        recipient = link;
        page.ancestor = serde_json::from_value(result["ancestor"].clone())?;
    }
    Err(io::Error::other(
        "The handoff chain exceeds the history traversal limit.",
    ))
}
