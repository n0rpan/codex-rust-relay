//! Only the credential-store reference appears in hosts.json.
use ox_host_web::client::Credential;
use serde::{Deserialize, Serialize};
use std::io;
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) enum Storage {
    Keychain,
    #[cfg(target_os = "linux")]
    PrivateFile,
}
#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use secret_service::{EncryptionType, Error, blocking::SecretService};
    use std::{
        collections::HashMap,
        fs,
        io::{Read, Write},
        os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        path::PathBuf,
    };
    fn service() -> Result<SecretService<'static>, Error> {
        SecretService::connect(EncryptionType::Dh)
    }
    fn unavailable(error: &Error) -> bool {
        match error {
            Error::Unavailable => true,
            Error::Zbus(zbus::Error::MethodError(name, _, _)) => {
                name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
            }
            Error::Zbus(zbus::Error::FDO(e)) => {
                matches!(e.as_ref(), zbus::fdo::Error::ServiceUnknown(_))
            }
            _ => false,
        }
    }
    fn attributes(id: &str) -> HashMap<&str, &str> {
        HashMap::from([("application", "dev.ox.desktop"), ("pairing", id)])
    }
    fn file(id: &str) -> io::Result<PathBuf> {
        if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(io::Error::other("Invalid pairing reference"));
        }
        let dir = super::super::directory()?.join("device-keys");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let m = dir.symlink_metadata()?;
        if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "Device key directory must be private to your user",
            ));
        }
        Ok(dir.join(id))
    }
    pub(crate) fn save(id: &str, credential: &Credential) -> io::Result<Storage> {
        match service() {
            Ok(s) => {
                let collection = match s.get_default_collection() {
                    Err(Error::NoResult) => s.create_collection("OX", "default"),
                    r => r,
                }
                .map_err(io::Error::other)?;
                if collection.is_locked().map_err(io::Error::other)? {
                    collection.unlock().map_err(io::Error::other)?;
                }
                collection
                    .create_item(
                        "OX paired desktop",
                        attributes(id),
                        &serde_json::to_vec(credential)?,
                        false,
                        "application/json",
                    )
                    .map_err(io::Error::other)?;
                Ok(Storage::Keychain)
            }
            Err(e) if unavailable(&e) => {
                let dest = file(id)?;
                let mut tmp = tempfile::NamedTempFile::new_in(dest.parent().unwrap())?;
                serde_json::to_writer(&mut tmp, credential)?;
                tmp.flush()?;
                tmp.as_file().sync_all()?;
                tmp.persist_noclobber(dest)?;
                Ok(Storage::PrivateFile)
            }
            Err(e) => Err(io::Error::other(e)),
        }
    }
    pub(crate) fn load(id: &str, storage: Storage) -> io::Result<Credential> {
        let bytes = match storage {
            Storage::Keychain => {
                let s = service().map_err(io::Error::other)?;
                let hits = s.search_items(attributes(id)).map_err(io::Error::other)?;
                let item = if let Some(item) = hits.unlocked.into_iter().next() {
                    item
                } else {
                    let item = hits.locked.into_iter().next().ok_or_else(|| {
                        io::Error::other(
                            "Device key missing from Secret Service. Pair this host again.",
                        )
                    })?;
                    item.unlock().map_err(io::Error::other)?;
                    item
                };
                item.get_secret().map_err(io::Error::other)?
            }
            Storage::PrivateFile => {
                let f = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(file(id)?)?;
                let meta = f.metadata()?;
                if !meta.is_file()
                    || meta.uid() != unsafe { libc::geteuid() }
                    || meta.mode() & 0o077 != 0
                    || meta.len() > 8192
                {
                    return Err(io::Error::other("Invalid private device key file"));
                }
                let mut bytes = Vec::new();
                f.take(8193).read_to_end(&mut bytes)?;
                bytes
            }
        };
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub(crate) fn remove(id: &str, storage: Storage) -> io::Result<()> {
        match storage {
            Storage::PrivateFile => match fs::remove_file(file(id)?) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            },
            Storage::Keychain => {
                let s = service().map_err(io::Error::other)?;
                let hits = s.search_items(attributes(id)).map_err(io::Error::other)?;
                for item in hits.unlocked.into_iter().chain(hits.locked) {
                    item.delete().map_err(io::Error::other)?;
                }
                Ok(())
            }
        }
    }
}
#[cfg(target_os = "linux")]
pub(super) use linux::{load, remove, save};
#[cfg(target_os = "macos")]
const SERVICE: &str = "dev.ox.desktop.host-pairing";
#[cfg(target_os = "macos")]
pub(super) fn save(id: &str, credential: &Credential) -> io::Result<Storage> {
    security_framework::passwords::set_generic_password(
        SERVICE,
        id,
        &serde_json::to_vec(credential)?,
    )
    .map_err(io::Error::other)?;
    Ok(Storage::Keychain)
}
#[cfg(target_os = "macos")]
pub(super) fn load(id: &str, _: Storage) -> io::Result<Credential> {
    Ok(serde_json::from_slice(
        &security_framework::passwords::get_generic_password(SERVICE, id)
            .map_err(io::Error::other)?,
    )?)
}
#[cfg(target_os = "macos")]
pub(super) fn remove(id: &str, _: Storage) -> io::Result<()> {
    let bytes = match security_framework::passwords::get_generic_password(SERVICE, id) {
        Err(e) if e.code() == -25300 => return Ok(()), // errSecItemNotFound
        result => result.map_err(io::Error::other)?,
    };
    let credential: Credential = serde_json::from_slice(&bytes)?;
    credential.discard_key()?;
    security_framework::passwords::delete_generic_password(SERVICE, id).map_err(io::Error::other)
}
