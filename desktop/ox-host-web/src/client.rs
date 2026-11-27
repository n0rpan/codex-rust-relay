//! Native paired client of the same Serve endpoints as the phone.
use crate::terminal_protocol::{Event, Input, OpenTerminal};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use futures_util::{SinkExt, StreamExt};
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use reqwest::{Client, Response, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io, os::unix::net::UnixStream, sync::OnceLock, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

const COOKIE: &str = "__Host-ox-device";
pub(crate) type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
const LIMIT: usize = 64 * 1024 * 1024;

pub fn runtime() -> io::Result<&'static tokio::runtime::Runtime> {
    static RT: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
    })
    .as_ref()
    .map_err(|e| io::Error::other(e.clone()))
}

// The code is deliberately kept out of URL queries, command lines and diagnostics.
pub struct Invitation {
    pub origin: String,
    code: String,
}
impl Invitation {
    pub fn parse(address: &str, manual: &str) -> io::Result<Self> {
        let invitation = address.trim().split_once("#pair=");
        let (address, code) = match invitation {
            Some((origin, secret)) if manual.trim().is_empty() => (origin, secret),
            None => (address.trim(), manual.trim()),
            _ => {
                return Err(io::Error::other(
                    "Paste one invitation link or enter the host URL and code.",
                ));
            }
        };
        let raw = if address.contains("://") {
            address.to_owned()
        } else {
            format!("https://{address}")
        };
        let normalized = raw
            .trim_end_matches('/')
            .strip_suffix(":443")
            .unwrap_or(&raw);
        let mut origin = crate::serve::origin(normalized).ok_or_else(|| {
            io::Error::other("Enter the HTTPS .ts.net address printed by ox host pair.")
        })?;
        if invitation.is_none()
            && !address
                .trim_end_matches('/')
                .trim_start_matches("https://")
                .contains(':')
        {
            origin.push_str(":8443");
        }
        let code = if code.len() == 43
            && code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            code.to_owned()
        } else {
            crate::admin::manual_code(code).ok_or_else(|| {
                io::Error::other("Enter the 12-character code or paste the full invitation link.")
            })?
        };
        Ok(Self { origin, code })
    }
}

/// Store only in the OS credential store or its private Linux fallback.
#[derive(Serialize, Deserialize)]
pub struct Credential {
    origin: String,
    key: SavedKey,
    token: String,
}
#[derive(Clone)]
pub struct PairedClient {
    origin: String,
    key: DeviceKey,
    cookie: header::HeaderValue,
    http: Client,
    #[cfg(any(test, feature = "fixture-transport"))]
    fixture: Option<(std::net::SocketAddr, tokio_tungstenite::Connector)>,
    settings_slots: std::sync::Arc<tokio::sync::Semaphore>,
}
fn http() -> io::Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)
}
async fn checked(response: Result<Response, reqwest::Error>, denied: &str) -> io::Result<Response> {
    let response = response.map_err(|e| {
        let kind = if e.is_timeout() {
            io::ErrorKind::TimedOut
        } else if e.is_connect() {
            io::ErrorKind::NotConnected
        } else {
            io::ErrorKind::ConnectionAborted
        };
        io::Error::new(kind, e.without_url())
    })?;
    match response.status() {
        s if s.is_success() => Ok(response),
        reqwest::StatusCode::BAD_REQUEST => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The OX host did not accept this request.",
        )),
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::UNPROCESSABLE_ENTITY => {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "This host does not recognize the request. Update OX on the host and try again.",
            ))
        }
        reqwest::StatusCode::TOO_MANY_REQUESTS => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "The host is busy. Try again shortly.",
        )),
        reqwest::StatusCode::FORBIDDEN => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            denied.to_owned(),
        )),
        s => Err(io::Error::other(format!("OX host returned {s}"))),
    }
}
#[derive(Serialize, Deserialize)]
enum SavedKey {
    Software(String),
    #[cfg(target_os = "macos")]
    Enclave(String),
}
#[derive(Clone)]
enum DeviceKey {
    Software(SigningKey),
    #[cfg(target_os = "macos")]
    Enclave(security_framework::key::SecKey),
}
impl DeviceKey {
    fn generate() -> io::Result<(Self, SavedKey)> {
        #[cfg(target_os = "macos")]
        {
            use security_framework::{
                item::Location,
                key::{GenerateKeyOptions, KeyType, SecKey, Token},
            };
            let mut options = GenerateKeyOptions::default();
            options
                .set_key_type(KeyType::ec_sec_prime_random())
                .set_size_in_bits(256)
                .set_token(Token::SecureEnclave)
                .set_location(Location::DataProtectionKeychain)
                .set_label("OX paired desktop");
            if let Ok(key) = SecKey::new(&options) {
                let id = key.application_label().ok_or_else(|| {
                    let _ = key.delete();
                    io::Error::other("Keychain did not return a device key reference")
                })?;
                return Ok((Self::Enclave(key), SavedKey::Enclave(B64.encode(id))));
            }
        }
        Ok(Self::software())
    }
    fn software() -> (Self, SavedKey) {
        let key = SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let saved = SavedKey::Software(B64.encode(key.to_bytes()));
        (Self::Software(key), saved)
    }
    fn load(saved: &SavedKey) -> io::Result<Self> {
        match saved {
            SavedKey::Software(text) => B64
                .decode(text)
                .ok()
                .and_then(|v| SigningKey::from_slice(&v).ok())
                .map(Self::Software)
                .ok_or_else(|| io::Error::other("Invalid saved device key")),
            #[cfg(target_os = "macos")]
            SavedKey::Enclave(id) => {
                use security_framework::item::{
                    ItemSearchOptions, KeyClass, Reference, SearchResult,
                };
                let id = B64.decode(id).map_err(io::Error::other)?;
                ItemSearchOptions::new()
                    .key_class(KeyClass::private())
                    .application_label(&id)
                    .ignore_legacy_keychains()
                    .load_refs(true)
                    .search()
                    .map_err(|err| {
                        if err.code() == -25300 {
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                "The paired Secure Enclave key is missing",
                            )
                        } else {
                            io::Error::other(err)
                        }
                    })?
                    .into_iter()
                    .find_map(|result| match result {
                        SearchResult::Ref(Reference::Key(key)) => Some(Self::Enclave(key)),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            "The paired Secure Enclave key is missing",
                        )
                    })
            }
        }
    }
    fn discard(&self) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        if let Self::Enclave(key) = self {
            return match key.delete() {
                Err(err) if err.code() == -25300 => Ok(()),
                result => result.map_err(io::Error::other),
            };
        }
        Ok(())
    }
    fn public(&self) -> io::Result<String> {
        match self {
            Self::Software(key) => {
                Ok(B64.encode(key.verifying_key().to_encoded_point(false).as_bytes()))
            }
            #[cfg(target_os = "macos")]
            Self::Enclave(key) => key
                .public_key()
                .and_then(|k| k.external_representation())
                .map(|p| B64.encode(p.bytes()))
                .ok_or_else(|| io::Error::other("Cannot read public device key")),
        }
    }
    fn sign(&self, text: &str) -> io::Result<String> {
        let sig: Signature = match self {
            Self::Software(key) => key.sign(text.as_bytes()),
            #[cfg(target_os = "macos")]
            Self::Enclave(key) => {
                let der = key
                    .create_signature(
                        security_framework::key::Algorithm::ECDSASignatureMessageX962SHA256,
                        text.as_bytes(),
                    )
                    .map_err(|e| io::Error::other(e.to_string()))?;
                Signature::from_der(&der).map_err(io::Error::other)?
            }
        };
        Ok(B64.encode(sig.to_bytes()))
    }
}
pub async fn register(invite: Invitation, label: &str) -> io::Result<Credential> {
    register_as(invite, label, None).await
}
pub(crate) async fn register_as(
    invite: Invitation,
    label: &str,
    source: Option<&str>,
) -> io::Result<Credential> {
    #[cfg(any(test, feature = "fixture-transport"))]
    let test_transport = fixture_transport::transport(&invite.origin);
    #[cfg(any(test, feature = "fixture-transport"))]
    let (http, (key, saved)) = match test_transport {
        Some((http, _, _)) => (http, DeviceKey::software()),
        None => (http()?, DeviceKey::generate()?),
    };
    #[cfg(not(any(test, feature = "fixture-transport")))]
    let (http, (key, saved)) = (http()?, DeviceKey::generate()?);
    let result: io::Result<String> = async {
        let (class, message, refusal) = if let Some(source) = source {
            ("host", format!("{}.host.{source}", invite.code),
             "Worker pairing was refused. Use a fresh worker invitation from the receiving host (ox host pair --worker-host). Both hosts need the invited Tailscale login.")
        } else {
            ("desktop", invite.code.clone(),
             "Pairing was refused. Check your Tailscale login and get a fresh invitation with ox host pair.")
        };
        let mut payload = json!({"code":invite.code, "signature":key.sign(&message)?,
            "key":key.public()?, "class":class,"label":label});
        if let Some(source) = source { payload["source"] = json!(source); }
        let response = checked(
            http.post(format!("{}/api/pair", invite.origin))
                .header(header::ORIGIN, &invite.origin).json(&payload).send().await,
            refusal,
        ).await?;
        response.headers().get_all(header::SET_COOKIE).iter()
            .filter_map(|v| v.to_str().ok()?.split(';').next()?.split_once('='))
            .find(|(k,t)| *k == COOKIE && B64.decode(t).is_ok_and(|b| b.len() == 32))
            .map(|(_, token)| token.to_owned())
            .ok_or_else(|| io::Error::other("The host did not return a device credential"))
    }.await;
    let token = result.inspect_err(|_| {
        let _ = key.discard();
    })?;
    Ok(Credential {
        origin: invite.origin,
        key: saved,
        token,
    })
}
impl Credential {
    pub fn discard_key(&self) -> io::Result<()> {
        match DeviceKey::load(&self.key) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result?.discard(),
        }
    }
}

/// An OX host another machine can see on the tailnet, and whether it is
/// waiting for a device to pair right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenHost {
    pub origin: String,
    pub name: String,
    pub tags: Vec<String>,
    pub expires_at: i64,
}

/// Ask a host whether its pairing window is open. Only the window's state
/// crosses the tailnet; the login it admits stays private.
pub(crate) async fn window_status(origin: &str) -> io::Result<Option<(String, i64)>> {
    let http = Client::builder()
        .timeout(Duration::from_secs(4))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)?;
    let response = http
        .post(format!("{origin}/api/pair/window/status"))
        .header(header::ORIGIN, origin)
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::NotConnected, e.without_url()))?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let value: Value = response.json().await.map_err(io::Error::other)?;
    Ok(value["open"].as_bool().filter(|open| *open).map(|_| {
        (
            value["name"].as_str().unwrap_or_default().to_owned(),
            value["expires_at"].as_i64().unwrap_or_default(),
        )
    }))
}

/// Enrol in a host whose pairing window the operator opened, instead of
/// carrying an invitation to it. The host only queues the request: the
/// operator there accepts it, and this waits for that decision.
pub(crate) async fn register_window(
    origin: &str,
    label: &str,
    source: Option<&str>,
) -> io::Result<Credential> {
    let (http, (key, saved)) = (http()?, DeviceKey::generate()?);
    let result: io::Result<String> = async {
        let class = if source.is_some() { "host" } else { "desktop" };
        let mut payload = json!({"key":key.public()?, "class":class, "label":label});
        if let Some(source) = source {
            payload["source"] = json!(source);
        }
        let response = checked(
            http.post(format!("{origin}/api/pair/window"))
                .header(header::ORIGIN, origin)
                .json(&payload)
                .send()
                .await,
            "That host is not waiting for a device to pair. Open a pairing window there and try again.",
        )
        .await?;
        let request: Value = response.json().await.map_err(io::Error::other)?;
        let id = request["request"]
            .as_str()
            .ok_or_else(|| io::Error::other("The host did not name the pairing request"))?;
        // The phone and the desktop both wrap this in a one-minute settings
        // request, so wait inside that rather than outliving the caller.
        let until = std::time::Instant::now() + Duration::from_secs(50);
        loop {
            let response = http
                .post(format!("{origin}/api/pair/window/poll"))
                .header(header::ORIGIN, origin)
                .json(&json!({"request": id}))
                .send()
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::NotConnected, e.without_url()))?;
            if response.status() == reqwest::StatusCode::NO_CONTENT {
                return response
                    .headers()
                    .get_all(header::SET_COOKIE)
                    .iter()
                    .filter_map(|v| v.to_str().ok()?.split(';').next()?.split_once('='))
                    .find(|(k, t)| *k == COOKIE && B64.decode(t).is_ok_and(|b| b.len() == 32))
                    .map(|(_, token)| token.to_owned())
                    .ok_or_else(|| {
                        io::Error::other("The host did not return a device credential")
                    });
            }
            if response.status() != reqwest::StatusCode::ACCEPTED {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "The host declined this device or the request expired.",
                ));
            }
            if std::time::Instant::now() >= until {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("No one accepted this device on {origin}. Approve it there and try again."),
                ));
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    }
    .await;
    let token = result.inspect_err(|_| {
        let _ = key.discard();
    })?;
    Ok(Credential {
        origin: origin.to_owned(),
        key: saved,
        token,
    })
}
impl PairedClient {
    pub async fn worker_request(&self, call: Value) -> io::Result<Value> {
        let request = call.to_string();
        let proof = self.proof("worker", Some(&request)).await?;
        self.post(
            "worker-link",
            json!({"request":request,"proof":proof}),
            "Worker link was revoked or is no longer valid.",
            Duration::from_secs(12),
        )
        .await
    }
    pub async fn revoke_self(&self) -> io::Result<()> {
        let devices = self.settings(json!({"action":"devices"})).await?;
        let id = devices["self"]
            .as_str()
            .ok_or_else(|| io::Error::other("Host did not identify this desktop"))?;
        self.settings(json!({"action":"revoke_device","id":id}))
            .await?;
        Ok(())
    }
    pub fn new(origin: &str, credential: &Credential) -> io::Result<Self> {
        let origin = crate::serve::origin(origin)
            .ok_or_else(|| io::Error::other("Invalid saved host address"))?;
        if origin != credential.origin {
            return Err(io::Error::other(
                "This device credential belongs to another host address",
            ));
        }
        let key = DeviceKey::load(&credential.key)?;
        let mut cookie = header::HeaderValue::from_str(&format!("{COOKIE}={}", credential.token))
            .map_err(io::Error::other)?;
        cookie.set_sensitive(true);
        #[cfg(any(test, feature = "fixture-transport"))]
        let (http, fixture) = match fixture_transport::transport(&origin) {
            Some((http, addr, connector)) => (http, Some((addr, connector))),
            None => (http()?, None),
        };
        #[cfg(not(any(test, feature = "fixture-transport")))]
        let http = http()?;
        Ok(Self {
            origin,
            key,
            cookie,
            http,
            #[cfg(any(test, feature = "fixture-transport"))]
            fixture,
            settings_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
        })
    }
    pub(crate) async fn post(
        &self,
        path: &str,
        data: Value,
        denied: &str,
        timeout: Duration,
    ) -> io::Result<Value> {
        let reply = checked(
            self.http
                .post(format!("{}/api/{path}", self.origin))
                .timeout(timeout)
                .header(header::ORIGIN, &self.origin)
                .header(header::COOKIE, self.cookie.clone())
                .json(&data)
                .send()
                .await,
            denied,
        )
        .await?;
        reply.json().await.map_err(io::Error::other)
    }
    pub(crate) async fn proof(&self, purpose: &str, request: Option<&str>) -> io::Result<Value> {
        let reply = self
            .post(
                "challenge",
                json!({"purpose":purpose}),
                if purpose == "worker" {
                    "The worker link is no longer available. Reconnect it with a fresh worker invitation."
                } else if purpose == "terminal" {
                    "On the host, run ox host devices, then ox host terminal <device> on."
                } else {
                    "This desktop needs to pair again. Run ox host pair on the host."
                },
                Duration::from_secs(12),
            )
            .await?;
        let nonce = reply["nonce"]
            .as_str()
            .filter(|s| s.len() == 43)
            .ok_or_else(|| io::Error::other("Invalid device challenge"))?;
        let message = request.map_or_else(
            || nonce.to_owned(),
            |text| format!("{nonce}.{}", B64.encode(Sha256::digest(text.as_bytes()))),
        );
        Ok(json!({"nonce":nonce,"signature":self.key.sign(&message)?}))
    }
    pub async fn settings(&self, action: Value) -> io::Result<Value> {
        let timeout = match action["action"].as_str() {
            Some(
                "add_worker_link"
                | "add_worker_link_window"
                | "link_paired_host"
                | "remove_worker_link"
                | "pair_key_destination"
                | "review_key_transfer"
                | "send_key_transfer",
            ) => Duration::from_secs(75),
            _ => Duration::from_secs(12),
        };
        let request = action.to_string();
        let _slot = self
            .settings_slots
            .acquire()
            .await
            .map_err(io::Error::other)?;
        let mut delay = Duration::from_millis(250);
        let response = loop {
            let attempt = async {
                let proof = self.proof("settings", Some(&request)).await?;
                self.post(
                    "settings",
                    json!({"request":request,"proof":proof}),
                    "The host refused this settings request.",
                    timeout,
                )
                .await
            }
            .await;
            match attempt {
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock && delay <= Duration::from_secs(2) =>
                {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                result => break result?,
            }
        };
        if let Some(message) = response["error"].as_str() {
            return Err(io::Error::other(message.to_owned()));
        }
        Ok(response)
    }
    pub(crate) async fn websocket(&self, path: &str, first: Value) -> io::Result<Socket> {
        let mut req = format!(
            "wss://{}/api/{path}",
            self.origin.trim_start_matches("https://")
        )
        .into_client_request()
        .map_err(io::Error::other)?;
        req.headers_mut().insert(
            header::ORIGIN,
            self.origin.parse().map_err(io::Error::other)?,
        );
        req.headers_mut()
            .insert(header::COOKIE, self.cookie.clone());
        let connect = async {
            #[cfg(any(test, feature = "fixture-transport"))]
            if let Some((addr, tls)) = &self.fixture {
                let stream = tokio::net::TcpStream::connect(addr).await?;
                return tokio_tungstenite::client_async_tls_with_config(
                    req,
                    stream,
                    None,
                    Some(tls.clone()),
                )
                .await;
            }
            tokio_tungstenite::connect_async(req).await
        };
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(12), connect)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        socket
            .send(Message::text(first.to_string()))
            .await
            .map_err(io::Error::other)?;
        Ok(socket)
    }

    /// The PTY belongs to the remote operator endpoint. Closing its input owner closes the socket.
    pub async fn terminal(
        &self,
        request: OpenTerminal,
    ) -> io::Result<(flume::Sender<Input>, flume::Receiver<Event>)> {
        let request = serde_json::to_string(&request)?;
        let proof = self.proof("terminal", Some(&request)).await?;
        let socket = self
            .websocket("operator", json!({"proof":proof,"request":request}))
            .await?;
        let (keys, input) = flume::bounded::<Input>(64);
        let (updates, events) = flume::bounded(8);
        let (mut out, mut incoming) = socket.split();
        tokio::spawn(async move {
            let send = async {
                while let Ok(frame) = input.recv_async().await {
                    out.send(Message::text(serde_json::to_string(&frame)?))
                        .await
                        .map_err(io::Error::other)?;
                }
                Ok::<(), io::Error>(())
            };
            let receive = async {
                while let Some(frame) = incoming.next().await {
                    match frame.map_err(io::Error::other)? {
                        Message::Text(text) => updates
                            .send_async(serde_json::from_str(&text)?)
                            .await
                            .map_err(io::Error::other)?,
                        Message::Ping(_) | Message::Pong(_) => (),
                        _ => break,
                    }
                }
                Ok::<(), io::Error>(())
            };
            tokio::select! {_=send=>(),_=receive=>()}
        });
        Ok((keys, events))
    }

    /// Translate WebSocket message boundaries to the desktop reader's NDJSON stream.
    /// Dropping the desktop stream ends the task without affecting any runtime on the host.
    pub async fn session(&self) -> io::Result<UnixStream> {
        let proof = self.proof("connect", None).await?;
        let socket = self.websocket("socket", proof).await?;
        let (desktop, bridge) = UnixStream::pair()?;
        bridge.set_nonblocking(true)?;
        let (input, mut output) = tokio::net::UnixStream::from_std(bridge)?.into_split();
        let (mut writer, mut reader) = socket.split();
        tokio::spawn(async move {
            let outgoing = async {
                let mut input = BufReader::new(input);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    if (&mut input)
                        .take(LIMIT as u64 + 1)
                        .read_until(b'\n', &mut line)
                        .await?
                        == 0
                    {
                        break;
                    }
                    if line.len() > LIMIT || line.pop() != Some(b'\n') {
                        break;
                    }
                    let text = std::str::from_utf8(&line).map_err(io::Error::other)?;
                    writer
                        .send(Message::text(text))
                        .await
                        .map_err(io::Error::other)?;
                }
                Ok::<(), io::Error>(())
            };
            let incoming = async {
                while let Some(msg) = reader.next().await {
                    match msg.map_err(io::Error::other)? {
                        Message::Text(data) if data.len() <= LIMIT => {
                            output.write_all(data.as_bytes()).await?;
                            if !data.ends_with('\n') {
                                output.write_all(b"\n").await?;
                            }
                        }
                        Message::Ping(_) | Message::Pong(_) => (),
                        _ => break,
                    }
                }
                Ok::<(), io::Error>(())
            };
            tokio::select! { _ = outgoing => (), _ = incoming => () }
        });
        Ok(desktop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_pairing_defaults_to_serve_port_and_invites_preserve_their_origin() {
        for address in ["build.tail.ts.net", "https://build.tail.ts.net/"] {
            assert_eq!(
                Invitation::parse(address, "0123-4567-89AB").unwrap().origin,
                "https://build.tail.ts.net:8443"
            );
        }
        assert_eq!(
            Invitation::parse("https://build.tail.ts.net:443/", "0123-4567-89AB")
                .unwrap()
                .origin,
            "https://build.tail.ts.net"
        );
        assert!(
            Invitation::parse(
                &format!("https://build.tail.ts.net#pair={}", "a".repeat(43)),
                ""
            )
            .is_ok()
        );
        for origin in [
            "https://build.tail.ts.net",
            "https://build.tail.ts.net:9443",
        ] {
            let link = format!("{origin}/#pair={}", "a".repeat(43));
            assert_eq!(Invitation::parse(&link, "").unwrap().origin, origin);
        }
    }
}

#[cfg(any(test, feature = "fixture-transport"))]
mod fixture_transport;
#[cfg(feature = "fixture-transport")]
pub use fixture_transport::enable as enable_fixture_transport;
#[cfg(test)]
mod fixture;
