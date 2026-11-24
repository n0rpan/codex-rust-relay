use ox_host_service::{Paths, ensure_running};
use std::{
    fs,
    io::Write,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    process::Command,
};

#[test]
fn ensure_in_child() {
    if std::env::var_os("OX_TEST_SERVICE_CHILD").is_some() {
        ensure_running(&Paths::discover().unwrap()).unwrap();
    }
}

#[test]
fn simultaneous_desktops_start_one_daemon_that_survives_their_exit() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("runtime/host.sock");
    let executable = dir.path().join("ox-host");
    fs::write(
        &executable,
        r#"#!/usr/bin/python3
import os,socket,sys
path=sys.argv[sys.argv.index("--socket")+1]
with open(os.path.join(os.environ["HOME"],"starts"),"a") as log: log.write("start\n")
with socket.socket(socket.AF_UNIX) as s:
    s.bind(path)
    os.chmod(path,0o600)
    s.listen()
    while True:
        c,_=s.accept()
        with c:
            if c.recv(20)==b"fixture-stop": break
os.unlink(path)
"#,
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let attempts: Vec<_> = (0..8)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "ensure_in_child"])
                .env("OX_TEST_SERVICE_CHILD", "1")
                .env("HOME", dir.path())
                .env("XDG_CONFIG_HOME", dir.path().join("config"))
                .env("OX_HOST_SOCKET", &socket)
                .env("OX_HOST_STATE_DIR", dir.path().join("state"))
                .env("PATH", dir.path())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in attempts {
        assert!(c.wait().unwrap().success());
    }
    let mut daemon = UnixStream::connect(&socket).unwrap();
    daemon.write_all(b"fixture-stop").unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("starts")).unwrap(),
        "start\n"
    );
}
