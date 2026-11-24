use super::{Screen, Terminal};
use super::{TerminalCommand, quote};
use std::{
    ffi::OsString,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type State = (Terminal, Arc<Mutex<Screen>>, flume::Receiver<()>);
fn command(project: &Path, script: &str, args: &[&str]) -> State {
    let mut argv = vec!["-c".into(), script.into(), "fixture".into()];
    argv.extend(args.iter().map(OsString::from));
    Terminal::start(
        project.into(),
        TerminalCommand {
            program: "/bin/sh".into(),
            args: argv,
        },
        Box::new(|| Ok(())),
        (24, 80),
    )
    .unwrap()
}
fn wait(screen: &Mutex<Screen>, updates: &flume::Receiver<()>, until: impl Fn(&Screen) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if until(&screen.lock().unwrap()) {
            return;
        }
        updates
            .recv_deadline(deadline)
            .expect("PTY did not reach its expected state");
    }
}
fn output(s: &Screen) -> String {
    s.parser.screen().contents()
}
fn code(s: &Screen) -> Option<i32> {
    s.outcome
        .as_ref()
        .and_then(|r| *r.as_ref().expect("terminal failed"))
}

#[test]
fn observes_echo_changes_even_when_the_child_writes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let (tty, screen, events) = command(
        temp.path(),
        "stty echo; read -r first; stty -echo; read -r secret; stty echo; read -r last",
        &[],
    );
    tty.observe_echo();
    wait(&screen, &events, |s| s.echo == Some(true));
    assert!(tty.send(b"first\r".to_vec()));
    wait(&screen, &events, |s| s.echo == Some(false));
    assert!(tty.send(b"private-fixture\r".to_vec()));
    wait(&screen, &events, |s| s.echo == Some(true));
    assert!(tty.send(b"last\r".to_vec()));
    wait(&screen, &events, |s| s.outcome.is_some());
    assert!(!output(&screen.lock().unwrap()).contains("private-fixture"));
}

#[test]
fn real_terminal_honors_noecho_and_size() {
    let temp = tempfile::tempdir().unwrap();
    let (tty, screen, events) = command(
        temp.path(),
        "test -t 0 && test -t 1 && test -t 2 || exit 2; stty -echo; printf 'Credential>'; IFS= read -r pw </dev/tty; stty echo; test \"$pw\" = \"$1\" || exit 3; stty size; printf 'accepted'; exit 17",
        &["fixture-private-canary"],
    );
    wait(&screen, &events, |s| output(s).contains("Credential>"));
    tty.resize((31, 91));
    wait(&screen, &events, |s| s.parser.screen().size() == (31, 91));
    assert!(tty.send(b"fixture-private-canary\r".to_vec()));
    wait(&screen, &events, |s| s.outcome.is_some());
    let s = screen.lock().unwrap();
    assert_eq!(code(&s), Some(17));
    let text = output(&s);
    assert!(
        text.contains("accepted") && text.contains("31 91"),
        "{text}"
    );
    assert!(!text.contains("fixture-private-canary"));
}

#[test]
fn copied_preview_and_direct_argv_preserve_literal_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let values = [
        "a b",
        "a'b",
        "$HOME;echo ignored",
        "line one\nline\ttwo\n",
        "",
    ];
    let script = "printf '%s\\0' \"$@\" > received; pwd > directory";
    let (tty, screen, updates) = command(temp.path(), script, &values);
    wait(&screen, &updates, |s| s.outcome.is_some());
    assert_eq!(code(&screen.lock().unwrap()), Some(0));
    drop(tty);
    let expected = values
        .iter()
        .flat_map(|v| v.bytes().chain([0]))
        .collect::<Vec<_>>();
    assert_eq!(
        std::fs::read(temp.path().join("received")).unwrap(),
        expected
    );
    assert_eq!(
        Path::new(
            std::fs::read_to_string(temp.path().join("directory"))
                .unwrap()
                .trim()
        )
        .canonicalize()
        .unwrap(),
        temp.path().canonicalize().unwrap()
    );
    let mut args = vec!["-c".into(), script.into(), "fixture".into()];
    args.extend(values.iter().map(OsString::from));
    let preview = TerminalCommand {
        program: "/bin/sh".into(),
        args,
    }
    .preview()
    .unwrap();
    assert!(
        Command::new("/bin/sh")
            .arg("-c")
            .arg(preview)
            .current_dir(temp.path())
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        std::fs::read(temp.path().join("received")).unwrap(),
        expected
    );
    assert!(quote("hidden\x1b[2J".as_ref()).is_err());
    assert!(quote("hidden\u{202e}abc".as_ref()).is_err());
}

#[test]
fn interrupt_and_eof_go_through_terminal_line_discipline() {
    let temp = tempfile::tempdir().unwrap();
    let (tty, s, u) = command(
        temp.path(),
        "trap 'exit 23' INT; printf 'waiting'; read -r response",
        &[],
    );
    wait(&s, &u, |s| output(s).contains("waiting"));
    assert!(tty.send(vec![3]));
    wait(&s, &u, |s| s.outcome.is_some());
    assert_eq!(code(&s.lock().unwrap()), Some(23));
    let (tty, s, u) = command(temp.path(), "printf 'waiting'; exec cat", &[]);
    wait(&s, &u, |s| output(s).contains("waiting"));
    assert!(tty.send(vec![4]));
    wait(&s, &u, |s| s.outcome.is_some());
    assert_eq!(code(&s.lock().unwrap()), Some(0));
}

#[test]
fn closing_panel_terminates_background_job_group() {
    let temp = tempfile::tempdir().unwrap();
    let (tty, s, u) = command(
        temp.path(),
        "set -m; sleep 90 & echo $! > job; printf ready; wait",
        &[],
    );
    wait(&s, &u, |s| output(s).contains("ready"));
    let pid = std::fs::read_to_string(temp.path().join("job")).unwrap();
    drop(tty);
    wait(&s, &u, |s| s.outcome.is_some());
    assert_eq!(code(&s.lock().unwrap()), None);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let found = Command::new("ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .unwrap();
        if !found.status.success()
            || String::from_utf8_lossy(&found.stdout)
                .trim()
                .starts_with('Z')
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Background terminal job survived closing the panel"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn rejected_preflight_cannot_start_program() {
    let temp = tempfile::tempdir().unwrap();
    let (_tty, s, u) = Terminal::start(
        temp.path().into(),
        TerminalCommand {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "touch marker".into()],
        },
        Box::new(|| Err("Mapping changed".into())),
        (24, 80),
    )
    .unwrap();
    wait(&s, &u, |s| s.outcome.is_some());
    assert_eq!(
        s.lock().unwrap().outcome,
        Some(Err("Mapping changed".into()))
    );
    assert!(!temp.path().join("marker").exists());
}
