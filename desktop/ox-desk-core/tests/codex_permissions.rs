//! What a stock Codex thread grants its workers, read by the real desktop store
//! from the installed app-server's own `thread/start` and
//! `thread/settings/updated`. Each case uses a private `CODEX_HOME` and a local
//! fixture provider that answers every turn with a canned reply: no model is
//! called. CI runs it for the floor and latest Codex.
use ox_desk_core::{
    links::codex::Link,
    modes::Level,
    runtime::{Access, Route},
    store::{Connection, Profile, Store},
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::Command,
    sync::mpsc,
    time::{Duration, Instant},
};

/// A Responses endpoint that completes every request with one short message.
fn provider() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut stream = stream;
                loop {
                    let (mut length, mut line) = (0usize, String::new());
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((k, v)) = line.split_once(':')
                            && k.eq_ignore_ascii_case("content-length")
                        {
                            length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0; length];
                    if reader.read_exact(&mut body).is_err() {
                        return;
                    }
                    let item = serde_json::json!({"type":"message","role":"assistant","id":"msg_1",
                        "status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]});
                    let events = [
                        serde_json::json!({"type":"response.created","response":{"id":"resp_1"}}),
                        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":item}),
                        serde_json::json!({"type":"response.completed","response":{"id":"resp_1","usage":{
                            "input_tokens":1,"output_tokens":1,"total_tokens":2,
                            "input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}),
                    ];
                    let sse: String = events
                        .iter()
                        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
                        .collect();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                        sse.len()
                    );
                    if stream.write_all(head.as_bytes()).is_err()
                        || stream.write_all(sse.as_bytes()).is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    port
}

struct Session {
    store: Store,
    rx: mpsc::Receiver<ox_desk_core::links::LinkEvent>,
    _home: tempfile::TempDir,
    _work: tempfile::TempDir,
}

impl Session {
    fn open(bin: &std::ffi::OsStr, config: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // `{extra}` names an existing directory outside the project.
        let extra = home.path().join("extra");
        std::fs::create_dir(&extra).unwrap();
        let config = config.replace("{extra}", &extra.to_string_lossy());
        std::fs::write(home.path().join("config.toml"), config).unwrap();
        let project = work.path().canonicalize().unwrap();
        let mut command = Command::new(bin);
        command
            .args([
                "-c",
                &format!(
                    "model_providers.fx={{name=\"fx\",base_url=\"http://127.0.0.1:{}/v1\",wire_api=\"responses\",env_key=\"FX_KEY\"}}",
                    provider()
                ),
                "-c",
                "model_provider=\"fx\"",
                "-c",
                "model=\"demo\"",
                "app-server",
                "--listen",
                "stdio://",
            ])
            .current_dir(&project)
            .env("CODEX_HOME", home.path())
            .env("FX_KEY", "fixture-only")
            .env("NO_PROXY", "127.0.0.1,localhost");
        for k in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
        ] {
            command.env_remove(k);
        }
        let (tx, rx) = mpsc::channel();
        let link = Link::from_command(command, move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        let store = Store::open(
            Profile {
                project,
                route: Route::codex_api("demo".into()),
                provider: "fx".into(),
            },
            link,
        )
        .unwrap();
        let mut session = Self {
            store,
            rx,
            _home: home,
            _work: work,
        };
        session.pump(|s| s.state() == &Connection::Ready && s.can_send(None));
        session
    }
    #[track_caller]
    fn pump(&mut self, until: impl Fn(&Store) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !until(&self.store) {
            assert!(
                Instant::now() < deadline,
                "timed out; state {:?} notice {:?}",
                self.store.state(),
                self.store.notice()
            );
            if let Ok(e) = self.rx.recv_timeout(Duration::from_millis(100)) {
                self.store.apply(e);
            }
            let _ = self.store.drain(vec![], Instant::now());
        }
    }
    /// Start a thread the way the desktop does and return its id.
    fn start(&mut self) -> String {
        self.store.edit(None, "permissions probe".into());
        self.store.submit(None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            assert!(Instant::now() < deadline, "no thread started");
            if let Ok(e) = self.rx.recv_timeout(Duration::from_millis(100)) {
                self.store.apply(e);
            }
            let _ = self.store.drain(vec![], Instant::now());
            if let Some(id) = self.store.take_opened() {
                self.pump(|s| s.applied_access(Some(&id)).is_some());
                return id;
            }
        }
    }
    fn facts(&self, id: &str) -> (Option<Access>, Option<Level>) {
        (
            self.store.applied_access(Some(id)),
            self.store.applied_ceiling(Some(id)),
        )
    }
}

#[test]
#[ignore = "CI runs against the floor and latest installed Codex binaries"]
fn configured_codex_threads_grant_what_their_settings_verify() {
    let bin = std::env::var_os("CODEOX_TEST_CODEX_BIN").expect("path to stock Codex binary");
    // The desktop starts threads with on-request approval reviewed by the user,
    // so the configured sandbox decides.
    for (config, access, ceiling) in [
        ("", Access::ReadOnly, Some(Level::Read)),
        (
            "sandbox_mode = \"workspace-write\"\n",
            Access::Workspace,
            Some(Level::Write),
        ),
        (
            "sandbox_mode = \"danger-full-access\"\n",
            Access::Configured,
            Some(Level::Classifier),
        ),
        (
            "sandbox_mode = \"workspace-write\"\n[sandbox_workspace_write]\nnetwork_access = true\nwritable_roots = [\"{extra}\"]\n",
            Access::Configured,
            Some(Level::Write),
        ),
        // Its children would write /tmp, which it may not.
        (
            "sandbox_mode = \"workspace-write\"\n[sandbox_workspace_write]\nexclude_slash_tmp = true\n",
            Access::Configured,
            Some(Level::Read),
        ),
        // A user-defined profile may restrict reads the reply cannot show.
        (
            "default_permissions = \"wide\"\n[permissions.wide]\nextends = \":workspace\"\n[permissions.wide.network]\nenabled = true\n",
            Access::Configured,
            None,
        ),
    ] {
        let mut session = Session::open(&bin, config);
        let id = session.start();
        assert_eq!(
            session.facts(&id),
            (Some(access), ceiling),
            "config {config:?}"
        );
    }

    // Choosing a preset is reported by thread/settings/updated and replaces both facts.
    let mut session = Session::open(&bin, "sandbox_mode = \"danger-full-access\"\n");
    let id = session.start();
    assert_eq!(
        session.facts(&id),
        (Some(Access::Configured), Some(Level::Classifier))
    );
    for (preset, level) in [
        (Access::ReadOnly, Level::Read),
        (Access::Workspace, Level::Write),
        (Access::AutoReview, Level::Classifier),
        (Access::Yolo, Level::Yolo),
    ] {
        session.pump(|s| s.can_change_access(&id));
        session.store.set_access(&id, preset).unwrap();
        session.pump(|s| s.applied_access(Some(&id)) == Some(preset));
        assert_eq!(session.facts(&id), (Some(preset), Some(level)));
    }
}
