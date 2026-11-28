//! The reader delivers stock output in wire order. Only the writer owns stdin.
use serde_json::Value;
use std::{
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

pub(super) struct Process {
    write: mpsc::Sender<Value>,
    close: mpsc::Sender<String>,
    reaped: ReapStatus,
}

#[derive(Clone, Default)]
pub(crate) struct ReapStatus(Arc<AtomicBool>);

impl ReapStatus {
    pub(crate) fn confirmed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl Process {
    pub(super) fn spawn(
        mut command: Command,
        mut frame: impl FnMut(&[u8]) -> io::Result<()> + Send + 'static,
        ready: Arc<AtomicBool>,
        closed: impl FnOnce(String, bool) + Send + 'static,
    ) -> io::Result<Self> {
        let mut child = command
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (write, messages) = mpsc::channel::<Value>();
        let (close, ends) = mpsc::channel::<String>();
        let mut out = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut input = BufWriter::new(child.stdin.take().expect("piped stdin"));
        let stderr = child.stderr.take().expect("piped stderr");
        let eof = close.clone();
        let reaped = ReapStatus::default();
        let child_reaped = reaped.clone();
        thread::spawn(move || {
            let mut line = Vec::new();
            let result = (|| -> io::Result<()> {
                loop {
                    line.clear();
                    const LIMIT: u64 = 64 * 1024 * 1024;
                    let n = Read::by_ref(&mut out)
                        .take(LIMIT + 1)
                        .read_until(b'\n', &mut line)?;
                    if n == 0 {
                        return Ok(());
                    }
                    if n as u64 > LIMIT {
                        return Err(io::Error::other(
                            "Claude Code output exceeded the frame limit",
                        ));
                    }
                    frame(&line)?;
                }
            })();
            let _ = eof.send(
                result
                    .err()
                    .map_or_else(|| "Claude Code closed its output".into(), |e| e.to_string()),
            );
        });
        let broken = close.clone();
        thread::spawn(move || {
            for value in messages {
                let r = serde_json::to_writer(&mut input, &value)
                    .map_err(io::Error::from)
                    .and_then(|_| input.write_all(b"\n"))
                    .and_then(|_| input.flush());
                if let Err(e) = r {
                    let _ = broken.send(e.to_string());
                    break;
                }
            }
        });
        // Stock stderr can contain arbitrary provider/project diagnostics. Recognize the
        // helper's account failure without forwarding potentially sensitive lines to the GUI.
        let diagnostics = thread::spawn(move || {
            let mut stream = BufReader::new(stderr);
            let mut bytes = Vec::new();
            let mut needs_login = false;
            loop {
                bytes.clear();
                if Read::by_ref(&mut stream)
                    .take(8192)
                    .read_until(b'\n', &mut bytes)
                    .unwrap_or(0)
                    == 0
                {
                    break;
                }
                needs_login |= String::from_utf8_lossy(&bytes)
                    .contains("no Claude subscription sign-in found. Run ox login claude first");
            }
            needs_login
        });
        thread::spawn(move || {
            let reason = match ends.recv_timeout(Duration::from_secs(30)) {
                Ok(reason) => reason,
                Err(mpsc::RecvTimeoutError::Timeout) if ready.load(Ordering::Acquire) => {
                    ends.recv().unwrap_or_else(|_| "Connection closed".into())
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    "Claude Code timed out initializing the session".into()
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => "Connection closed".into(),
            };
            ready.store(false, Ordering::Release);
            let group = -(child.id() as i32);
            // Keep the direct child unreaped until the last signal: its group ID cannot be recycled.
            unsafe {
                libc::kill(group, libc::SIGTERM);
            }
            thread::sleep(Duration::from_millis(250));
            unsafe {
                libc::kill(group, libc::SIGKILL);
            }
            child_reaped
                .0
                .store(child.wait().is_ok(), Ordering::Release);
            let needs_login = diagnostics.join().unwrap_or_default();
            closed(reason, needs_login);
        });
        Ok(Self {
            write,
            close,
            reaped,
        })
    }
    pub(super) fn reap_status(&self) -> ReapStatus {
        self.reaped.clone()
    }
    /// A handle the reader can use to answer a request without going through the link.
    pub(super) fn writer(&self) -> mpsc::Sender<Value> {
        self.write.clone()
    }
    pub(super) fn send(&self, value: Value) -> io::Result<()> {
        self.write
            .send(value)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Claude Code writer closed"))
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.close.send("Connection closed by desktop".into());
    }
}
