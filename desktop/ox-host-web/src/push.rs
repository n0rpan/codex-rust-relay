use crate::{bridge, server::App, state::now};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ox_desk_core::host_protocol::{Client, Cursor, NotificationKind, Scope, Server, VERSION};
use p256::{
    PublicKey, SecretKey,
    ecdsa::{Signature, SigningKey, signature::Signer},
    elliptic_curve::sec1::ToEncodedPoint,
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{io::BufReader, sync::mpsc};
use url::Url;
use web_push_native::{Auth, WebPushBuilder};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: Keys,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    pub p256dh: String,
    pub auth: String,
}
impl Subscription {
    fn url(&self) -> Option<Url> {
        if self.endpoint.len() > 4096 {
            return None;
        }
        let u = Url::parse(&self.endpoint).ok()?;
        let path = u.path();
        let allowed = match u.host_str()? {
            "fcm.googleapis.com" => path
                .strip_prefix("/fcm/send/")
                .or_else(|| path.strip_prefix("/wp/")),
            "updates.push.services.mozilla.com" => path
                .strip_prefix("/wpush/v2/")
                .or_else(|| path.strip_prefix("/wpush/v1/")),
            "web.push.apple.com" => path.strip_prefix('/'),
            _ => None,
        }?;
        (u.scheme() == "https"
            && u.port().is_none()
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none()
            && u.query().is_none()
            && u.as_str() == self.endpoint
            && !allowed.is_empty()
            && allowed
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_:=".contains(&b)))
        .then_some(u)
    }
    fn keys(&self) -> Option<(PublicKey, [u8; 16])> {
        let key = B64.decode(&self.keys.p256dh).ok()?;
        if key.len() != 65 {
            return None;
        }
        Some((
            PublicKey::from_sec1_bytes(&key).ok()?,
            B64.decode(&self.keys.auth).ok()?.try_into().ok()?,
        ))
    }
    pub fn valid(&self) -> bool {
        self.url().is_some() && self.keys().is_some()
    }
}
#[derive(Serialize)]
struct Payload<'a> {
    thread: &'a str,
    title: &'a str,
    kind: NotificationKind,
}
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let [x, y, z, _] = a.octets();
            !(x == 0
                || x == 10
                || x == 127
                || x >= 224
                || x == 169 && y == 254
                || x == 172 && (16..32).contains(&y)
                || x == 192 && (y == 168 || y == 0 || y == 2 || y == 88 && z == 99)
                || x == 100 && (64..128).contains(&y)
                || x == 198 && (y == 18 || y == 19 || y == 51 && z == 100)
                || x == 203 && y == 0 && z == 113)
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && (s[1] <= 0x1ff || s[1] == 0xdb8) || s[0] == 0x2002)
        }
    }
}
fn encrypt(
    sub: &Subscription,
    key: &SecretKey,
    data: &[u8],
    contact: &str,
) -> Option<(Url, reqwest::header::HeaderMap, Vec<u8>)> {
    let url = sub.url()?;
    let (public, auth) = sub.keys()?;
    let encrypted = WebPushBuilder::new(url.as_str().parse().ok()?, public, Auth::from(auth))
        .with_valid_duration(Duration::from_secs(3600))
        .build(data)
        .ok()?;
    let (parts, body) = encrypted.into_parts();
    let claims=B64.encode(serde_json::to_vec(&serde_json::json!({"aud":url.origin().ascii_serialization(),"exp":now()+3600,"sub":contact})).ok()?);
    let signing = format!(
        "{}.{}",
        B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#),
        claims
    );
    let signature: Signature = SigningKey::from(key).sign(signing.as_bytes());
    let auth = format!(
        "vapid t={signing}.{}, k={}",
        B64.encode(signature.to_bytes()),
        B64.encode(key.public_key().to_encoded_point(false).as_bytes())
    );
    let mut headers = parts.headers;
    headers.insert("authorization", auth.parse().ok()?);
    Some((url, headers, body))
}
async fn send(sub: &Subscription, key: &SecretKey, data: &[u8], contact: &str) -> Option<u16> {
    let (url, headers, body) = encrypt(sub, key, data, contact)?;
    let host = url.host_str()?;
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, 443)).await.ok()?.collect();
    if addrs.is_empty() || addrs.iter().any(|addr| !public_ip(addr.ip())) {
        return None;
    }
    let client = client_builder()
        .resolve_to_addrs(host, &addrs)
        .build()
        .ok()?;
    post(client, url, headers, body).await
}
fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
}
async fn post(
    client: reqwest::Client,
    url: Url,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
) -> Option<u16> {
    Some(
        client
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .ok()?
            .status()
            .as_u16(),
    )
}
pub(crate) async fn watch(app: App) {
    let mut wake = app.state.lock().unwrap().changed.subscribe();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);
    let worker = app.clone();
    tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            let (key, subs, origin) = {
                let mut s = worker.state.lock().unwrap();
                (s.vapid(), s.subscriptions(), s.configuration())
            };
            let (Ok(key), Some((contact, _))) = (key, origin) else {
                continue;
            };
            for (id, sub) in subs {
                let mut changed = worker.state.lock().unwrap().changed.subscribe();
                let deliver =
                    tokio::time::timeout(Duration::from_secs(7), send(&sub, &key, &data, &contact));
                tokio::pin!(deliver);
                loop {
                    if !worker
                        .state
                        .lock()
                        .unwrap()
                        .is_subscribed(&id, &sub.endpoint)
                    {
                        break;
                    }
                    tokio::select! {biased;
                        _=changed.changed()=>continue,
                        status=&mut deliver=>{
                            if matches!(status,Ok(Some(404|410))) {
                                let mut state=worker.state.lock().unwrap();
                                if state.is_subscribed(&id,&sub.endpoint){let _=state.set_push(&id,None);}
                            }
                            break;
                        }
                    }
                }
            }
        }
    });
    let mut cursor = None;
    loop {
        if app.state.lock().unwrap().subscriptions().is_empty() {
            cursor = None;
            let _ = wake.changed().await;
            continue;
        }
        tokio::select! {
            _=wake.changed()=>{},
            _=notifications(&app,&mut cursor,&tx)=>{tokio::select!{_=tokio::time::sleep(Duration::from_secs(3))=>{},_=wake.changed()=>{}}}
        }
    }
}
async fn notifications(
    app: &App,
    cursor: &mut Option<Cursor>,
    tx: &mpsc::Sender<Vec<u8>>,
) -> io::Result<()> {
    let host = bridge::connect(&app.host).await?;
    let (r, mut w) = host.into_split();
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    bridge::write(&mut w, &Client::Hello { version: VERSION }).await?;
    if !matches!(
        serde_json::from_slice::<Server>(&bridge::line(&mut r, &mut buf).await?)?,
        Server::Hello {
            version: VERSION,
            ..
        }
    ) {
        return Err(io::Error::other("Host version mismatch"));
    }
    bridge::write(
        &mut w,
        &Client::Subscribe {
            scope: None,
            after: cursor.clone(),
        },
    )
    .await?;
    loop {
        let bytes = bridge::line(&mut r, &mut buf).await?;
        match notices(&bytes, cursor.as_ref())? {
            Notices::Skip => {}
            Notices::Snapshot(c) => *cursor = Some(c),
            Notices::Delta(c, payloads) => {
                for data in payloads {
                    if tx.send(data).await.is_err() {
                        return Ok(());
                    }
                }
                *cursor = Some(c);
            }
        }
    }
}
enum Notices {
    Skip,
    Snapshot(Cursor),
    Delta(Cursor, Vec<Vec<u8>>),
}
/// Turn one host frame into Web Push payloads. Each payload holds only the scoped
/// thread identifier, the session title and the event kind.
fn notices(bytes: &[u8], cursor: Option<&Cursor>) -> io::Result<Notices> {
    let frame: NoticeFrame = serde_json::from_slice(bytes)?;
    let Some(c) = frame.cursor else {
        if frame.kind == "error" {
            return Err(io::Error::other("Host subscription closed"));
        }
        return Ok(Notices::Skip);
    };
    if frame.kind == "snapshot" {
        return Ok(Notices::Snapshot(c));
    }
    if frame.kind != "delta" || cursor.is_some_and(|old| old.epoch == c.epoch && old.seq >= c.seq) {
        return Ok(Notices::Skip);
    }
    let Some(scope) = frame.scope else {
        return Ok(Notices::Skip);
    };
    let mut payloads = Vec::new();
    for change in frame.changes {
        if change.kind != "notification" {
            continue;
        }
        let (Some(thread), Some(title), Some(kind)) = (change.thread, change.title, change.event)
        else {
            continue;
        };
        let title = title
            .chars()
            .filter(|c| !c.is_control())
            .take(80)
            .collect::<String>();
        let alias = match &scope.route.billing {
            ox_desk_core::runtime::Billing::Subscription => None,
            ox_desk_core::runtime::Billing::Api(a) => Some(a.as_str()),
        };
        let identity = serde_json::to_vec(&(&scope.project, scope.route.runtime, alias, &thread))?;
        let hash = ring::digest::digest(&ring::digest::SHA256, &identity);
        let thread = format!("{thread}~{}", B64.encode(hash.as_ref()));
        payloads.push(serde_json::to_vec(&Payload {
            thread: &thread,
            title: &title,
            kind,
        })?);
    }
    Ok(Notices::Delta(c, payloads))
}

// Push never materializes transcript bodies or tool output from a global subscription.
#[derive(Deserialize)]
struct NoticeFrame {
    #[serde(rename = "type")]
    kind: String,
    cursor: Option<Cursor>,
    scope: Option<Scope>,
    #[serde(default)]
    changes: Vec<NoticeChange>,
}
#[derive(Deserialize)]
struct NoticeChange {
    kind: String,
    thread: Option<String>,
    title: Option<String>,
    event: Option<NotificationKind>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{VerifyingKey, signature::Verifier};
    use p256::elliptic_curve::rand_core::OsRng;
    use serde_json::json;
    fn fixture() -> (Subscription, SecretKey) {
        let phone = SecretKey::random(&mut OsRng);
        (
            Subscription {
                endpoint: "https://updates.push.services.mozilla.com/wpush/v2/fixture".into(),
                keys: Keys {
                    p256dh: B64.encode(phone.public_key().to_encoded_point(false).as_bytes()),
                    auth: B64.encode([7; 16]),
                },
            },
            phone,
        )
    }
    #[test]
    fn encrypted_push_has_only_metadata_and_origin_scoped_vapid_signature() {
        let (sub, phone) = fixture();
        let vapid = SecretKey::random(&mut OsRng);
        let payload = br#"{"thread":"one","title":"OX session","kind":"approval"}"#;
        let (_, headers, body) =
            encrypt(&sub, &vapid, payload, "https://test.tail.ts.net").unwrap();
        assert!(!body.windows(7).any(|p| p == b"session"));
        let clear = web_push_native::decrypt(body, &phone, &Auth::from([7; 16])).unwrap();
        assert_eq!(clear, payload);
        let (token, key) = headers["authorization"]
            .to_str()
            .unwrap()
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        let (signed, sig) = token.rsplit_once('.').unwrap();
        let signature = Signature::from_slice(&B64.decode(sig).unwrap()).unwrap();
        VerifyingKey::from_sec1_bytes(&B64.decode(key).unwrap())
            .unwrap()
            .verify(signed.as_bytes(), &signature)
            .unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&B64.decode(signed.split_once('.').unwrap().1).unwrap())
                .unwrap();
        assert_eq!(claims["aud"], "https://updates.push.services.mozilla.com");
        assert!(claims["exp"].as_i64().unwrap() > now());
    }
    #[tokio::test]
    async fn push_delivers_an_encrypted_post_to_a_tls_service() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let dir = tempfile::tempdir().unwrap();
        let mut server = Command::new("python3")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/push_tls.py"))
            .arg(dir.path())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut port = String::new();
        BufReader::new(server.stdout.take().unwrap())
            .read_line(&mut port)
            .unwrap();
        let address = format!("127.0.0.1:{}", port.trim()).parse().unwrap();
        let (mut sub, phone) = fixture();
        sub.endpoint = "https://web.push.apple.com/synthetic".into();
        let (url, headers, body) = encrypt(
            &sub,
            &SecretKey::random(&mut OsRng),
            b"fixture",
            "https://host.fixture.ts.net",
        )
        .unwrap();
        let cert =
            reqwest::Certificate::from_pem(&std::fs::read(dir.path().join("ca.pem")).unwrap())
                .unwrap();
        let client = client_builder()
            .add_root_certificate(cert)
            .resolve("web.push.apple.com", address)
            .build()
            .unwrap();
        assert_eq!(post(client, url, headers, body).await, Some(201));
        assert!(server.wait().unwrap().success());
        let ciphertext = std::fs::read(dir.path().join("encrypted")).unwrap();
        assert_eq!(
            web_push_native::decrypt(ciphertext, &phone, &Auth::from([7; 16])).unwrap(),
            b"fixture"
        );
    }
    #[test]
    fn push_payloads_carry_only_scoped_thread_title_and_kind() {
        use ox_desk_core::{
            asks::{CommandApproval, Question},
            host_protocol::{AskView, Change, SessionView, Timeline, TimelineItem},
            runtime::{Billing, RequestId, Route, Runtime},
        };
        const SECRET: &str = "SECRET-7f3a";
        let leak = |what: &str| format!("{SECRET}-{what}");
        // Every kind must be listed here; adding one fails this match until it is covered.
        let all = [
            NotificationKind::Finished,
            NotificationKind::Failed,
            NotificationKind::Approval,
            NotificationKind::Question,
        ];
        for kind in all {
            match kind {
                NotificationKind::Finished
                | NotificationKind::Failed
                | NotificationKind::Approval
                | NotificationKind::Question => {}
            }
        }
        for (n, kind) in all.into_iter().enumerate() {
            let scope = Scope {
                project: format!("/home/me/{}", leak("project")).into(),
                route: Route {
                    runtime: Runtime::Claude,
                    billing: Billing::Api(leak("alias")),
                },
            };
            let question = Question {
                id: "q".into(),
                question: leak("question"),
                header: leak("header"),
                is_other: false,
                is_secret: true,
                multi_select: false,
                options: None,
            };
            let ask = AskView {
                command: Some(CommandApproval {
                    command: format!("curl -H 'Authorization: {}' https://x", leak("command")),
                    cwd: Some(format!("/srv/{}", leak("cwd")).into()),
                    decline: 1,
                }),
                id: RequestId::Text(leak("request")),
                token: leak("token"),
                thread: Some("thread-1".into()),
                turn: Some("turn".into()),
                item: Some("item".into()),
                choice_details: vec![Some(leak("choice-detail"))],
                title: format!("Run {}?", leak("ask-title")),
                detail: format!("--- a/x\n+++ b/x\n+{}", leak("diff")),
                choices: vec![leak("choice")],
                questions: if kind == NotificationKind::Question {
                    vec![question]
                } else {
                    vec![]
                },
            };
            let item = TimelineItem {
                turn: "turn".into(),
                id: "item".into(),
                role: "tool".into(),
                text: leak("message-body"),
                revision: 1,
                detail: json!({"arguments": {"path": leak("tool-argument")}, "diff": leak("diff")}),
            };
            let frame = Server::Delta {
                cursor: Cursor {
                    epoch: "e".into(),
                    seq: 1 + n as u64,
                },
                scope: scope.clone(),
                changes: vec![
                    Change::Asks { asks: vec![ask] },
                    Change::Session {
                        session: SessionView {
                            id: "thread-1".into(),
                            title: "Fix login".into(),
                            parent: None,
                            status: "running".into(),
                            updated_at: 0,
                            capabilities: vec![],
                            detail: json!({"last_error": leak("error")}),
                        },
                    },
                    Change::Transcript {
                        thread: "thread-1".into(),
                        timeline: Timeline {
                            items: vec![item.clone()],
                            detail: json!({"pending": leak("timeline")}),
                        },
                    },
                    Change::Items {
                        thread: "thread-1".into(),
                        items: vec![item],
                    },
                    Change::Append {
                        thread: "thread-1".into(),
                        turn: "turn".into(),
                        id: "item".into(),
                        text: leak("append"),
                        revision: 2,
                    },
                    Change::TimelineState {
                        thread: "thread-1".into(),
                        detail: json!({"command": leak("state")}),
                    },
                    Change::Notification {
                        thread: "thread-1".into(),
                        title: "Fix login\u{7}".into(),
                        event: kind,
                    },
                ],
            };
            let bytes = serde_json::to_vec(&frame).unwrap();
            assert!(String::from_utf8_lossy(&bytes).contains(SECRET));
            let Notices::Delta(_, payloads) = notices(&bytes, None).unwrap() else {
                panic!("delta frame was not processed");
            };
            assert_eq!(payloads.len(), 1, "{kind:?}");
            let text = String::from_utf8(payloads[0].clone()).unwrap();
            assert!(!text.contains(SECRET), "{kind:?} payload leaked: {text}");
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(keys, ["kind", "thread", "title"]);
            assert_eq!(v["kind"], serde_json::to_value(kind).unwrap());
            assert_eq!(v["title"], "Fix login");
            let thread = v["thread"].as_str().unwrap();
            assert!(thread.starts_with("thread-1~") && thread.len() == "thread-1~".len() + 43);
        }
    }
    #[test]
    fn push_endpoint_and_destination_are_constrained() {
        let (mut sub, _) = fixture();
        assert!(sub.valid());
        for endpoint in [
            "http://127.0.0.1/push",
            "https://user:pw@web.push.apple.com/abc",
            "https://updates.push.services.mozilla.com.evil.test/wpush/v2/x",
            "https://fcm.googleapis.com/v1/projects/me/messages:send",
            "https://web.push.apple.com:8443/path",
            "https://web.push.apple.com//evil",
            "https://web.push.apple.com/?injected=true",
        ] {
            sub.endpoint = endpoint.into();
            assert!(!sub.valid());
        }
        for ip in [
            "127.0.0.1",
            "100.85.2.3",
            "192.168.1.1",
            "10.0.0.1",
            "::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "fd7a:115c:a1e0::1",
            "2002:7f00:1::",
        ] {
            assert!(!public_ip(ip.parse().unwrap()));
        }
    }
}
