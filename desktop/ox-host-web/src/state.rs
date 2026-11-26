pub(crate) use crate::serve::origin;
use crate::{
    admin::{Device, DeviceClass, Pairing, Reply},
    push::Subscription,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use p256::{
    PublicKey, SecretKey,
    elliptic_curve::{rand_core::OsRng, sec1::ToEncodedPoint},
};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
    signature,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

/// A paired device that is not used for this long is forgotten.
pub(crate) const INACTIVE: i64 = 30 * 24 * 3600;
/// Lifetime of a single-use pairing invitation.
pub(crate) const PAIR_TTL: i64 = 300;
#[cfg(test)]
thread_local! {
    /// Tests pin the clock so expiry boundaries are checked exactly.
    pub(crate) static CLOCK: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}
pub(crate) fn now() -> i64 {
    #[cfg(test)]
    if let Some(t) = CLOCK.with(std::cell::Cell::get) {
        return t;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub(crate) fn random() -> String {
    let mut bytes = [0; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("OS random generator unavailable");
    B64.encode(bytes)
}
fn digest(s: &str) -> String {
    B64.encode(digest::digest(&digest::SHA256, s.as_bytes()).as_ref())
}
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "Remote state directory must be owned by this user with mode 0700",
        ));
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Registered {
    #[serde(default)]
    terminal_generation: u64,
    pub device: Device,
    token_hash: String,
    key: String,
    pub push: Option<Subscription>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Disk {
    origin: Option<String>,
    #[serde(default)]
    last_origin: Option<String>,
    port: u16,
    devices: HashMap<String, Registered>,
    vapid: Option<String>,
}
#[derive(Clone)]
struct Invite {
    terminal: bool,
    short_hash: String,
    login: String,
    issuer: Option<String>,
    expires: i64,
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Purpose {
    Connect,
    Push,
    Settings,
    Terminal,
}
struct Challenge {
    id: String,
    purpose: Purpose,
    expires: i64,
}
pub(crate) struct State {
    dir: PathBuf,
    disk: Disk,
    invites: HashMap<String, Invite>,
    challenges: HashMap<String, Challenge>,
    pub changed: watch::Sender<u64>,
}

impl State {
    pub fn load(dir: &Path) -> io::Result<Self> {
        private_dir(dir)?;
        let path = dir.join("remote.json");
        let disk = match fs::symlink_metadata(&path) {
            Ok(meta)
                if meta.is_file()
                    && meta.uid() == unsafe { libc::geteuid() }
                    && meta.mode() & 0o077 == 0
                    && meta.len() < 1024 * 1024 =>
            {
                serde_json::from_slice(&fs::read(path)?)?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Disk::default(),
            _ => {
                return Err(io::Error::other(
                    "Remote state has invalid permissions or size",
                ));
            }
        };
        if disk
            .origin
            .as_deref()
            .is_some_and(|value| origin(value).as_deref() != Some(value))
        {
            return Err(io::Error::other(
                "Stored remote origin is not a canonical HTTPS Serve address",
            ));
        }
        let mut state = Self {
            dir: dir.into(),
            disk,
            invites: HashMap::new(),
            challenges: HashMap::new(),
            changed: watch::channel(0).0,
        };
        if state
            .disk
            .devices
            .values()
            .any(|r| r.device.last_used + INACTIVE <= now())
        {
            state.update(|d| {
                d.devices
                    .retain(|_, r| r.device.last_used + INACTIVE > now())
            })?;
        }
        Ok(state)
    }
    fn update(&mut self, change: impl FnOnce(&mut Disk)) -> io::Result<()> {
        let mut disk = self.disk.clone();
        change(&mut disk);
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(&mut file, &disk)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(self.dir.join("remote.json"))
            .map_err(|e| e.error)?;
        self.disk = disk;
        Ok(())
    }
    pub fn configuration(&self) -> Option<(String, u16)> {
        Some((self.disk.origin.clone()?, self.disk.port))
    }
    pub fn set_origin(&mut self, origin: Option<String>, port: u16) -> io::Result<()> {
        if origin != self.disk.origin || origin.is_none() {
            self.update(|d| {
                if let Some(next) = &origin {
                    if d.last_origin.as_ref().or(d.origin.as_ref()) != Some(next) {
                        d.devices.clear();
                    }
                    d.last_origin = Some(next.clone());
                } else if let Some(prior) = &d.origin {
                    d.last_origin = Some(prior.clone());
                }
                d.origin = origin;
                d.port = port;
            })?;
            self.invites.clear();
            self.challenges.clear();
            self.signal();
        } else if port != self.disk.port {
            self.update(|d| d.port = port)?
        }
        Ok(())
    }
    fn signal(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1))
    }
    pub fn host_profile(
        &self,
        edit: Option<crate::host_profile::Edit>,
    ) -> io::Result<crate::host_profile::HostProfile> {
        match edit {
            Some(edit) => edit.save(&self.dir),
            None => crate::host_profile::HostProfile::read(&self.dir),
        }
    }
    pub fn reply(&self, pairing: Option<Pairing>) -> Reply {
        let mut devices: Vec<_> = self
            .disk
            .devices
            .values()
            .filter(|r| r.device.last_used + INACTIVE > now())
            .map(|r| r.device.clone())
            .collect();
        devices.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.id.cmp(&b.id)));
        Reply {
            update: None,
            profile: None,
            enabled: self.disk.origin.is_some(),
            origin: self.disk.origin.clone(),
            port: self.disk.port,
            devices,
            pairing,
            reservation: None,
        }
    }
    pub fn invite(&mut self, login: String, terminal: bool) -> io::Result<Pairing> {
        self.create_invite(login, None, terminal)
    }
    pub fn device_invite(&mut self, id: &str) -> io::Result<Pairing> {
        if !self.alive(id) {
            return Err(io::Error::other("Device is no longer paired"));
        }
        let login = self.disk.devices[id].device.login.clone();
        self.create_invite(login, Some(id.to_owned()), false)
    }
    fn create_invite(
        &mut self,
        login: String,
        issuer: Option<String>,
        terminal: bool,
    ) -> io::Result<Pairing> {
        let origin = self
            .disk
            .origin
            .as_ref()
            .ok_or_else(|| io::Error::other("Enable remote access first"))?;
        if login.is_empty()
            || login.len() > 254
            || !login.is_ascii()
            || login.bytes().any(|b| b.is_ascii_control() || b == b' ')
        {
            return Err(io::Error::other("Invalid Tailscale login"));
        }
        let code = random();
        // Twelve independent base32 symbols (60 bits), with ambiguous letters omitted.
        const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut bytes = [0u8; 12];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| io::Error::other("Pairing needs OS randomness"))?;
        let short: String = bytes
            .iter()
            .map(|b| char::from(ALPHABET[usize::from(b & 31)]))
            .collect();
        let expires_at = now() + PAIR_TTL;
        self.invites.retain(|_, v| v.expires > now());
        if self.invites.len() >= 16 {
            return Err(io::Error::other("Too many active pairing invitations"));
        }
        let url = format!("{origin}/#pair={code}");
        let qr_svg = qrcode::QrCode::new(&url)
            .map_err(io::Error::other)?
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(240, 240)
            .build();
        self.invites.insert(
            digest(&code),
            Invite {
                terminal,
                short_hash: digest(&short),
                issuer,
                login,
                expires: expires_at,
            },
        );
        Ok(Pairing {
            short_code: Some(format!("{}-{}-{}", &short[..4], &short[4..8], &short[8..])),
            url,
            qr_svg,
            expires_at,
        })
    }
    pub fn pair(
        &mut self,
        login: &str,
        code: &str,
        key: &str,
        signature: &str,
        label: &str,
        class: DeviceClass,
    ) -> Option<String> {
        if code.len() > 43
            || label.trim().is_empty()
            || label.chars().count() > 64
            || label.chars().any(char::is_control)
        {
            return None;
        }
        let h = if code.len() == 43 {
            digest(code)
        } else {
            let normalized = crate::admin::manual_code(code)?;
            let short_hash = digest(&normalized);
            self.invites
                .iter()
                .find(|(_, invite)| invite.short_hash == short_hash)?
                .0
                .clone()
        };
        let invitation = self.invites.get(&h)?;
        if invitation.login != login
            || invitation.expires <= now()
            || invitation
                .issuer
                .as_deref()
                .is_some_and(|id| !self.alive(id))
            || !verify(key, code, signature)
        {
            return None;
        }
        let t = random();
        let id = random();
        let registered = Registered {
            terminal_generation: 0,
            device: Device {
                class,
                terminal: invitation.terminal,
                id: id.clone(),
                login: login.into(),
                label: label.trim().into(),
                last_used: now(),
            },
            token_hash: digest(&t),
            key: key.into(),
            push: None,
        };
        if self.reply(None).devices.len() >= 64 {
            return None;
        }
        self.update(|d| {
            d.devices
                .retain(|_, r| r.device.last_used + INACTIVE > now());
            d.devices.insert(id, registered);
        })
        .ok()?;
        self.invites.remove(&h);
        Some(t)
    }
    pub fn identity(&self, login: &str, cookie: &str) -> Option<String> {
        if self.disk.origin.is_none() || cookie.len() != 43 {
            return None;
        }
        let hash = digest(cookie);
        self.disk
            .devices
            .values()
            .find(|r| {
                r.token_hash == hash
                    && r.device.login == login
                    && r.device.last_used + INACTIVE > now()
            })
            .map(|r| r.device.id.clone())
    }
    pub fn alive(&self, id: &str) -> bool {
        self.disk.origin.is_some()
            && self
                .disk
                .devices
                .get(id)
                .is_some_and(|r| r.device.last_used + INACTIVE > now())
    }
    pub fn device_class(&self, id: &str) -> Option<DeviceClass> {
        Some(self.disk.devices.get(id)?.device.class)
    }
    pub fn active_device(&mut self, id: &str) -> bool {
        if !self.alive(id) {
            return false;
        }
        if self.disk.devices[id].device.last_used + 3600 < now() {
            let _ = self.update(|d| d.devices.get_mut(id).unwrap().device.last_used = now());
        }
        true
    }
    pub fn terminal_generation(&self, id: &str) -> Option<u64> {
        let r = self.disk.devices.get(id)?;
        (self.alive(id) && r.device.terminal).then_some(r.terminal_generation)
    }
    pub fn terminal_grant(&mut self, id: &str, enabled: bool) -> io::Result<()> {
        let r = self
            .disk
            .devices
            .get(id)
            .ok_or_else(|| io::Error::other("Unknown paired device"))?;
        if r.device.terminal == enabled {
            return Ok(());
        }
        let generation = r
            .terminal_generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Device generation exhausted"))?;
        self.update(|disk| {
            let r = disk.devices.get_mut(id).expect("registered device");
            r.device.terminal = enabled;
            r.terminal_generation = generation;
        })?;
        self.challenges
            .retain(|_, c| c.id != id || c.purpose != Purpose::Terminal);
        self.signal();
        Ok(())
    }
    pub fn revoke(&mut self, id: &str) -> io::Result<()> {
        self.update(|d| {
            d.devices.remove(id);
        })?;
        self.challenges.retain(|_, c| c.id != id);
        self.invites
            .retain(|_, invite| invite.issuer.as_deref() != Some(id));
        self.signal();
        Ok(())
    }
    pub fn challenge(&mut self, id: &str, purpose: Purpose) -> Option<String> {
        self.challenges.retain(|_, c| c.expires > now());
        if !self.alive(id)
            || self.challenges.len() >= 256
            || self.challenges.values().filter(|c| c.id == id).count() >= 8
        {
            return None;
        }
        let nonce = random();
        self.challenges.insert(
            nonce.clone(),
            Challenge {
                id: id.into(),
                purpose,
                expires: now() + 60,
            },
        );
        Some(nonce)
    }
    pub fn prove(&mut self, id: &str, purpose: Purpose, nonce: &str, sig: &str) -> bool {
        self.prove_message(id, purpose, nonce, sig, nonce)
    }
    pub fn prove_settings(&mut self, id: &str, nonce: &str, sig: &str, request: &str) -> bool {
        let message = format!("{nonce}.{}", digest(request));
        self.prove_message(id, Purpose::Settings, nonce, sig, &message)
    }
    pub fn prove_terminal(
        &mut self,
        id: &str,
        nonce: &str,
        sig: &str,
        request: &str,
    ) -> Option<u64> {
        let grant = self.terminal_generation(id)?;
        self.prove_message(
            id,
            Purpose::Terminal,
            nonce,
            sig,
            &format!("{nonce}.{}", digest(request)),
        )
        .then_some(grant)
    }
    fn prove_message(
        &mut self,
        id: &str,
        purpose: Purpose,
        nonce: &str,
        sig: &str,
        message: &str,
    ) -> bool {
        let Some(c) = self.challenges.remove(nonce) else {
            return false;
        };
        let Some(r) = self.disk.devices.get(id) else {
            return false;
        };
        if c.id != id
            || c.purpose != purpose
            || c.expires <= now()
            || !self.alive(id)
            || !verify(&r.key, message, sig)
        {
            return false;
        }
        if r.device.last_used + 3600 < now() {
            let _ = self.update(|d| d.devices.get_mut(id).unwrap().device.last_used = now());
        }
        true
    }
    pub fn vapid(&mut self) -> io::Result<SecretKey> {
        if self.disk.vapid.is_none() {
            let key = SecretKey::random(&mut OsRng);
            self.update(|d| d.vapid = Some(B64.encode(key.to_bytes())))?
        }
        let secret = B64
            .decode(self.disk.vapid.as_ref().unwrap())
            .map_err(io::Error::other)?;
        SecretKey::from_slice(&secret).map_err(|_| io::Error::other("Invalid stored Web Push key"))
    }
    pub fn public_key(&mut self) -> io::Result<String> {
        Ok(B64.encode(
            self.vapid()?
                .public_key()
                .to_encoded_point(false)
                .as_bytes(),
        ))
    }
    pub fn set_push(&mut self, id: &str, push: Option<Subscription>) -> io::Result<()> {
        if !self.alive(id) {
            return Err(io::Error::other("Device expired"));
        }
        self.update(|d| d.devices.get_mut(id).unwrap().push = push)?;
        self.signal();
        Ok(())
    }
    pub fn is_subscribed(&self, id: &str, subscription: &Subscription) -> bool {
        self.alive(id)
            && self.disk.devices.get(id).and_then(|r| r.push.as_ref()) == Some(subscription)
    }
    pub fn subscriptions(&self) -> Vec<(String, Subscription)> {
        self.disk
            .devices
            .iter()
            .filter(|(id, _)| self.alive(id))
            .filter_map(|(id, r)| Some((id.clone(), r.push.clone()?)))
            .collect()
    }
}
fn verify(key: &str, challenge: &str, sig: &str) -> bool {
    let (Ok(k), Ok(s)) = (B64.decode(key), B64.decode(sig)) else {
        return false;
    };
    if k.len() != 65 || s.len() != 64 || PublicKey::from_sec1_bytes(&k).is_err() {
        return false;
    }
    signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, k)
        .verify(challenge.as_bytes(), &s)
        .is_ok()
}
#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{Signature, SigningKey, signature::Signer};
    fn sign(k: &SigningKey, nonce: &str) -> String {
        let sig: Signature = k.sign(nonce.as_bytes());
        B64.encode(sig.to_bytes())
    }
    #[test]
    fn identity_one_time_challenges_and_device_recovery() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let mut s = State::load(dir.path()).unwrap();
        s.set_origin(Some("https://machine.tail.ts.net".into()), 48162)
            .unwrap();
        let invite = s.invite("a@example.test".into(), false).unwrap();
        assert!(invite.qr_svg.contains("<svg"));
        let code = invite.url.split_once("#pair=").unwrap().1;
        let key = SigningKey::random(&mut OsRng);
        let public = B64.encode(key.verifying_key().to_encoded_point(false).as_bytes());
        assert!(
            s.pair(
                "b@example.test",
                code,
                &public,
                &sign(&key, code),
                "Phone",
                DeviceClass::Phone
            )
            .is_none()
        );
        let cookie = s
            .pair(
                "a@example.test",
                code,
                &public,
                &sign(&key, code),
                "Phone",
                DeviceClass::Phone,
            )
            .unwrap();
        assert!(
            s.pair(
                "a@example.test",
                code,
                &public,
                &sign(&key, code),
                "Replay",
                DeviceClass::Phone
            )
            .is_none()
        );
        let contents = fs::read_to_string(dir.path().join("remote.json")).unwrap();
        assert!(!contents.contains(&cookie) && !contents.contains(code));
        let mut s = State::load(dir.path()).unwrap();
        let id = s.identity("a@example.test", &cookie).unwrap();
        assert!(s.identity("b@example.test", &cookie).is_none());
        let nonce = s.challenge(&id, Purpose::Connect).unwrap();
        assert!(!s.prove(&id, Purpose::Push, &nonce, &sign(&key, &nonce)));
        let nonce = s.challenge(&id, Purpose::Connect).unwrap();
        let signed = sign(&key, &nonce);
        assert!(s.prove(&id, Purpose::Connect, &nonce, &signed));
        assert!(!s.prove(&id, Purpose::Connect, &nonce, &signed));
        let request = r#"{"action":"read_rows"}"#;
        let nonce = s.challenge(&id, Purpose::Settings).unwrap();
        let signed = sign(&key, &format!("{nonce}.{}", digest(request)));
        assert!(!s.prove_settings(&id, &nonce, &signed, r#"{"action":"read_advanced"}"#));
        let nonce = s.challenge(&id, Purpose::Settings).unwrap();
        let signed = sign(&key, &format!("{nonce}.{}", digest(request)));
        assert!(s.prove_settings(&id, &nonce, &signed, request));
        assert!(!s.prove_settings(&id, &nonce, &signed, request));
        let invitation = s.device_invite(&id).unwrap();
        let pending_code = invitation.url.split_once("#pair=").unwrap().1;
        let next = s.challenge(&id, Purpose::Connect).unwrap();
        s.revoke(&id).unwrap();
        assert!(
            s.pair(
                "a@example.test",
                pending_code,
                &public,
                &sign(&key, pending_code),
                "Withdrawn invitation",
                DeviceClass::Phone
            )
            .is_none()
        );
        assert!(!s.prove(&id, Purpose::Connect, &next, &sign(&key, &next)));
        assert!(s.identity("a@example.test", &cookie).is_none());
    }
    #[test]
    fn manual_code_and_long_link_spend_one_desktop_invitation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let mut state = State::load(dir.path()).unwrap();
        state
            .set_origin(Some("https://machine.tail.ts.net".into()), 48162)
            .unwrap();
        let login = "operator@example.test";
        let invite = state.invite(login.into(), false).unwrap();
        let short = invite.short_code.unwrap().to_lowercase();
        let long = invite.url.split_once("#pair=").unwrap().1;
        let key = SigningKey::random(&mut OsRng);
        let public = B64.encode(key.verifying_key().to_encoded_point(false).as_bytes());
        let pair = |state: &mut State, login: &str, code: &str| {
            state.pair(
                login,
                code,
                &public,
                &sign(&key, code),
                "Laptop",
                DeviceClass::Desktop,
            )
        };
        assert!(pair(&mut state, "other@example.test", &short).is_none());
        let token = pair(&mut state, login, &short).unwrap();
        assert!(pair(&mut state, login, long).is_none());
        let restored = State::load(dir.path()).unwrap();
        let id = restored.identity(login, &token).unwrap();
        assert_eq!(restored.device_class(&id), Some(DeviceClass::Desktop));
        assert!(restored.terminal_generation(&id).is_none());
    }

    #[test]
    fn terminal_grants_reject_stale_challenges_and_do_not_transfer_to_repaired_devices() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            tmp.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let mut state = State::load(tmp.path()).unwrap();
        state
            .set_origin(Some("https://machine.test.ts.net".into()), 9000)
            .unwrap();
        let key = SigningKey::random(&mut OsRng);
        let public = B64.encode(key.verifying_key().to_encoded_point(false).as_bytes());
        let register = |s: &mut State, invitation: Pairing| {
            let secret = invitation.url.split_once("#pair=").unwrap().1;
            let cookie = s
                .pair(
                    "tester",
                    secret,
                    &public,
                    &sign(&key, secret),
                    "Phone",
                    DeviceClass::Phone,
                )
                .unwrap();
            s.identity("tester", &cookie).unwrap()
        };
        let invitation = state.invite("tester".into(), false).unwrap();
        let id = register(&mut state, invitation);
        assert!(state.terminal_generation(&id).is_none());
        state.terminal_grant(&id, true).unwrap();
        let old = state.terminal_generation(&id).unwrap();
        let nonce = state.challenge(&id, Purpose::Terminal).unwrap();
        state.terminal_grant(&id, false).unwrap();
        state.terminal_grant(&id, true).unwrap();
        assert_ne!(state.terminal_generation(&id).unwrap(), old);
        let request = r#"{"preset":"shell"}"#;
        let sig = sign(&key, &format!("{nonce}.{}", digest(request)));
        assert!(state.prove_terminal(&id, &nonce, &sig, request).is_none());
        let nonce = state.challenge(&id, Purpose::Terminal).unwrap();
        let sig = sign(&key, &format!("{nonce}.{}", digest(request)));
        assert!(
            state
                .prove_terminal(&id, &nonce, &sig, r#"{"preset":"claude-login"}"#)
                .is_none()
        );
        let preauthorized = state.invite("tester".into(), true).unwrap();
        let trusted = register(&mut state, preauthorized);
        assert!(state.terminal_generation(&trusted).is_some());
        let next = state.device_invite(&trusted).unwrap();
        let paired = register(&mut state, next);
        assert_ne!(id, paired);
        assert!(state.terminal_generation(&paired).is_none());
        assert!(state.terminal_generation(&id).is_some());
        state.revoke(&id).unwrap();
        assert!(state.terminal_generation(&id).is_none());
    }
    #[test]
    fn expired_invitations_and_inactive_devices_cannot_be_used() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let mut state = State::load(dir.path()).unwrap();
        let origin = "https://machine.test.ts.net";
        state.set_origin(Some(origin.into()), 48162).unwrap();
        let key = SigningKey::random(&mut OsRng);
        let public = B64.encode(key.verifying_key().to_encoded_point(false).as_bytes());
        let invite = state.invite("me".into(), false).unwrap();
        let secret = invite.url.split_once("#pair=").unwrap().1;
        state.invites.get_mut(&digest(secret)).unwrap().expires = now() - 1;
        assert!(
            state
                .pair(
                    "me",
                    secret,
                    &public,
                    &sign(&key, secret),
                    "Phone",
                    DeviceClass::Phone
                )
                .is_none()
        );
        let invitation = state.invite("me".into(), false).unwrap();
        let secret = invitation.url.split_once("#pair=").unwrap().1;
        let cookie = state
            .pair(
                "me",
                secret,
                &public,
                &sign(&key, secret),
                "Phone",
                DeviceClass::Phone,
            )
            .unwrap();
        let id = state.identity("me", &cookie).unwrap();
        state.set_origin(None, 0).unwrap();
        assert!(state.identity("me", &cookie).is_none());
        state.set_origin(Some(origin.into()), 48162).unwrap();
        assert_eq!(state.identity("me", &cookie).as_deref(), Some(id.as_str()));
        state
            .update(|disk| {
                disk.devices.get_mut(&id).unwrap().device.last_used = now() - INACTIVE - 3
            })
            .unwrap();
        assert!(!state.active_device(&id));
        assert!(state.identity("me", &cookie).is_none());
        let reopened = State::load(dir.path()).unwrap();
        assert!(reopened.disk.devices.is_empty());
    }
    #[test]
    fn pairing_invitations_and_unused_pairings_expire_on_a_pinned_clock() {
        struct Unpin;
        impl Drop for Unpin {
            fn drop(&mut self) {
                CLOCK.with(|c| c.set(None));
            }
        }
        let _unpin = Unpin;
        let at = |t: i64| CLOCK.with(|c| c.set(Some(t)));
        const { assert!(PAIR_TTL <= 300, "invitations must stay short-lived") };
        assert_eq!(INACTIVE, 30 * 24 * 3600, "docs promise a 30-day expiry");
        let start = 1_900_000_000;
        at(start);
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let mut s = State::load(dir.path()).unwrap();
        s.set_origin(Some("https://machine.test.ts.net".into()), 9000)
            .unwrap();
        let key = SigningKey::random(&mut OsRng);
        let public = B64.encode(key.verifying_key().to_encoded_point(false).as_bytes());
        let pair = |s: &mut State, code: &str, label: &str| {
            s.pair(
                "me",
                code,
                &public,
                &sign(&key, code),
                label,
                DeviceClass::Phone,
            )
        };

        // An invitation expires exactly PAIR_TTL seconds after it is issued.
        let stale = s.invite("me".into(), false).unwrap();
        assert_eq!(stale.expires_at, start + PAIR_TTL);
        let stale = stale.url.split_once("#pair=").unwrap().1.to_owned();
        at(start + PAIR_TTL);
        assert!(pair(&mut s, &stale, "Late").is_none());

        // A fresh invitation works once, one second before its deadline, then never again.
        let issued = start + PAIR_TTL;
        let fresh = s.invite("me".into(), false).unwrap();
        let fresh = fresh.url.split_once("#pair=").unwrap().1.to_owned();
        at(issued + PAIR_TTL - 1);
        let cookie = pair(&mut s, &fresh, "Phone").unwrap();
        assert!(pair(&mut s, &fresh, "Replay").is_none());
        let paired = issued + PAIR_TTL - 1;
        let id = s.identity("me", &cookie).unwrap();

        // A pairing that is not used for 30 days expires, in memory and on reload.
        at(paired + INACTIVE - 1);
        assert_eq!(s.identity("me", &cookie).as_deref(), Some(id.as_str()));
        at(paired + INACTIVE);
        assert!(s.identity("me", &cookie).is_none());
        assert!(!s.alive(&id));
        assert!(s.reply(None).devices.is_empty());
        assert!(State::load(dir.path()).unwrap().disk.devices.is_empty());

        // Use renews the window: a device used on day 29 survives past day 30.
        let fresh = s.invite("me".into(), false).unwrap();
        let fresh = fresh.url.split_once("#pair=").unwrap().1.to_owned();
        let cookie = pair(&mut s, &fresh, "Phone").unwrap();
        let id = s.identity("me", &cookie).unwrap();
        let paired = paired + INACTIVE;
        at(paired + 29 * 24 * 3600);
        assert!(s.active_device(&id));
        at(paired + 31 * 24 * 3600);
        assert_eq!(s.identity("me", &cookie).as_deref(), Some(id.as_str()));
    }
    #[test]
    fn only_canonical_https_serve_origins_are_valid() {
        for v in [
            "http://machine.tail.ts.net",
            "https://trusted.ts.net@example.test",
            "https://machine.ts.net/out",
            "https://machine.ts.net.invalid",
            "https://127.0.0.1",
            "https://machine.ts.net?secret=1",
        ] {
            assert!(origin(v).is_none());
        }
        assert_eq!(
            origin("https://machine.tail.ts.net:8443/").as_deref(),
            Some("https://machine.tail.ts.net:8443")
        );
    }
}
