//! Invoked by tests/native_client.py with a disposable CA and private host.
use super::*;
use ox_desk_core::host_protocol::{Client as Frame, Server, VERSION};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use std::{
    env, fs,
    io::{BufRead, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

fn read(stream: &mut std::io::BufReader<UnixStream>) -> Server {
    let mut text = String::new();
    stream.read_line(&mut text).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
#[ignore = "launched by the isolated native client fixture"]
fn native_desktop() {
    let origin = env::var("OX_TEST_ORIGIN").unwrap();
    let addr: SocketAddr = env::var("OX_TEST_LOOPBACK").unwrap().parse().unwrap();
    assert!(addr.ip().is_loopback());
    let root = PathBuf::from(env::var_os("OX_TEST_NATIVE_DIR").unwrap());
    let pem = fs::read(root.join("ca.pem")).unwrap();
    let cert = CertificateDer::from_pem_slice(&pem).unwrap();
    let mut anchors = rustls::RootCertStore::empty();
    anchors.add(cert).unwrap();
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(anchors)
        .with_no_client_auth();
    let url = url::Url::parse(&origin).unwrap();
    let http = Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_pem(&pem).unwrap())
        .resolve(url.host_str().unwrap(), addr)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let invite =
        Invitation::parse(&origin, &fs::read_to_string(root.join("code")).unwrap()).unwrap();
    let rt = runtime().unwrap();
    let credential = rt
        .block_on(register_via(
            invite,
            "Native fixture",
            http.clone(),
            DeviceKey::software(),
        ))
        .unwrap();
    let mut client = PairedClient::new(&origin, &credential).unwrap();
    client.http = http;
    client.fixture = Some((addr, tokio_tungstenite::Connector::Rustls(Arc::new(config))));
    let handshake = || {
        let socket = rt.block_on(client.session()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut stream = std::io::BufReader::new(socket);
        writeln!(
            stream.get_mut(),
            "{}",
            serde_json::to_string(&Frame::Hello { version: VERSION }).unwrap()
        )
        .unwrap();
        let Server::Hello {
            host: Some(host),
            version: VERSION,
            ..
        } = read(&mut stream)
        else {
            panic!("expected compatible host");
        };
        (host.id.unwrap(), stream)
    };
    let (id, mut stream) = handshake();
    assert_eq!(id, fs::read_to_string(root.join("host-id")).unwrap().trim());
    writeln!(
        stream.get_mut(),
        "{}",
        serde_json::to_string(&Frame::WatchCatalog).unwrap()
    )
    .unwrap();
    assert!(matches!(read(&mut stream), Server::Snapshot { .. }));
    drop(stream);
    assert_eq!(handshake().0, id);
    let settings = |action| rt.block_on(client.settings(action)).unwrap();
    let devices = settings(json!({"action":"devices"}));
    assert_eq!(devices["devices"][0]["class"], "desktop");
    let snapshot = settings(json!({"action":"read_rows"}));
    assert_eq!(
        settings(
            json!({"action":"import_keys","revision":snapshot["revisions"]["keys"], "keys":[{
                "name":"BRAVE_SEARCH_API_KEY","scope":"https://api.search.brave.com/","value":"fixture-native-brave"
            }]})
        )["status"],
        "applied"
    );
    let route = ox_desk_core::store::Route::CODEX_SUBSCRIPTION;
    let project = root.join("workspace");
    assert_eq!(
        settings(json!({"action":"choose_project","route":route,"path":project}))["project"],
        project.to_str().unwrap()
    );
    if env::var_os("OX_TEST_STOCK_TURNS").is_some() {
        for (runtime, alias) in [("claude", "fixture/demo"), ("codex", "garden/demo")] {
            let path = root.join("workspace").join(runtime);
            fs::create_dir(&path).unwrap();
            let scope = json!({"project":path,"route":{"runtime":runtime,"billing":{"api":alias}}});
            settings(json!({"action":"choose_project","route":scope["route"],"path":path}));
            stock_turn(|| handshake().1, scope, &path);
        }
    }
    let (input, events) = rt
        .block_on(client.terminal(OpenTerminal::Shell { project }))
        .unwrap();
    let next = || events.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(next(), Event::Review { .. }));
    input.send(Input::Run).unwrap();
    assert!(matches!(next(), Event::Running { running: true }));
    input
        .send(Input::Keys {
            text: "exit 7\r".into(),
        })
        .unwrap();
    while !matches!(next(), Event::Exit { exit: Some(7) }) {}
    rt.block_on(client.revoke_self()).unwrap();
    assert!(rt.block_on(client.session()).is_err());
    credential.discard_key().unwrap();
}

fn stock_turn(
    connect: impl Fn() -> std::io::BufReader<UnixStream>,
    scope: Value,
    project: &std::path::Path,
) {
    fn send(stream: &mut std::io::BufReader<UnixStream>, frame: Value) {
        writeln!(stream.get_mut(), "{frame}").unwrap()
    }
    fn next(stream: &mut std::io::BufReader<UnixStream>) -> Value {
        let frame = serde_json::to_value(read(stream)).unwrap();
        assert_ne!(frame["type"], "error", "{}", frame["message"]);
        frame
    }
    let subscribe = |stream: &mut std::io::BufReader<UnixStream>| {
        stream
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(75)))
            .unwrap();
        send(
            stream,
            json!({"type":"subscribe","scope":scope,"terminal":false,"after":null}),
        );
    };
    let mut stream = connect();
    subscribe(&mut stream);
    loop {
        let msg = next(&mut stream);
        let ready = |v: &Value| {
            v["capabilities"]
                .as_array()
                .is_some_and(|a| a.contains(&json!("start")))
        };
        if msg["scopes"]
            .as_array()
            .is_some_and(|a| a.iter().any(ready))
            || msg["changes"]
                .as_array()
                .is_some_and(|a| a.iter().any(ready))
        {
            break;
        }
    }
    send(
        &mut stream,
        json!({"type":"act","id":"stock-start","scope":scope,"approval":null,"action":{"send_text":[null,"APPROVAL on the remote host"]}}),
    );
    let ask = loop {
        let msg = next(&mut stream);
        if let Some(ask) = msg["changes"]
            .as_array()
            .and_then(|a| a.iter().find_map(|c| c["asks"].get(0)))
        {
            break ask.clone();
        }
    };
    drop(stream);
    let mut stream = connect();
    subscribe(&mut stream);
    let pending = loop {
        let frame = next(&mut stream);
        if let Some(scopes) = frame["scopes"].as_array() {
            break scopes
                .iter()
                .find(|s| s["scope"] == scope)
                .and_then(|s| s["asks"].as_array())
                .is_some_and(|asks| {
                    asks.iter()
                        .any(|a| a["id"] == ask["id"] && a["token"] == ask["token"])
                });
        }
    };
    assert!(pending, "the same approval must survive reconnect");
    send(
        &mut stream,
        json!({"type":"act","id":"stock-accept","scope":scope,"approval":ask["token"],"action":{"answer_ask":[ask["id"],0]}}),
    );
    loop {
        let msg = next(&mut stream);
        if msg["changes"].as_array().is_some_and(|a| {
            a.iter().any(|c| {
                c["kind"] == "notification"
                    && c["event"] == "finished"
                    && c["thread"] == ask["thread"]
            })
        }) {
            break;
        }
    }
    assert_eq!(
        fs::read_to_string(project.join("approved.txt"))
            .unwrap()
            .trim(),
        "accepted"
    );
}
