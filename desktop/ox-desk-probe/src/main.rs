//! Sample Linux process counters independently of the GPUI event loop.
#[cfg(target_os = "linux")]
mod wayland_pointer;
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashSet},
    env, fs, io,
    path::Path,
};

fn invalid(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail.into())
}

fn field(text: &str, key: &str) -> io::Result<u64> {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| *name == key)
        .and_then(|(_, value)| value.split_whitespace().next()?.parse().ok())
        .ok_or_else(|| invalid(format!("missing {key}")))
}

fn sample(root: &Path, pid: u32) -> io::Result<(Value, BTreeSet<u32>)> {
    let dir = root.join(pid.to_string());
    let memory = fs::read_to_string(dir.join("smaps_rollup"))?;
    let stat = fs::read_to_string(dir.join("stat"))?;
    // comm is enclosed in parentheses and may itself contain spaces or parentheses.
    let (_, tail) = stat
        .rsplit_once(") ")
        .ok_or_else(|| invalid("missing comm"))?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    let counter = |i: usize| {
        fields
            .get(i)
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| invalid("invalid process stat"))
    };
    let mut switches = 0;
    let mut threads = 0;
    let mut children = BTreeSet::new();
    for task in fs::read_dir(dir.join("task"))? {
        let task = task?.path();
        let Some(status) = read_live_task(&task.join("status"))? else {
            continue;
        };
        switches += field(&status, "voluntary_ctxt_switches")?;
        threads += 1;
        if let Some(pids) = read_live_task(&task.join("children"))? {
            children.extend(
                pids.split_whitespace()
                    .filter_map(|p| p.parse::<u32>().ok()),
            );
        }
    }
    Ok((
        json!({
            "pid":pid, "start_ticks":counter(19)?, "cpu_ticks":counter(11)? + counter(12)?,
            "rss_kib": field(&memory, "Rss")?, "pss_kib":field(&memory, "Pss")?,
            "voluntary_context_switches": switches, "threads": threads,
        }),
        children,
    ))
}

fn process_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn read_live_task(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if process_gone(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

fn main() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    if env::args().nth(1).as_deref() == Some("wayland-pointer") {
        return wayland_pointer::run();
    }
    if env::args().nth(1).as_deref() == Some("gate") {
        use std::os::unix::process::CommandExt;
        return Err(std::process::Command::new("python3")
            .arg("-c")
            .arg(concat!(
                include_str!("../../tests/host_fixture.py"),
                "\n",
                include_str!("gate.py")
            ))
            .arg("--probe-commit")
            .arg(option_env!("OX_BUILD_SHA").unwrap_or_default())
            .arg("--limits-toml")
            .arg(include_str!("../../gate.toml"))
            .arg("--probe")
            .arg(env::current_exe()?)
            .args(env::args_os().skip(2))
            .exec());
    }
    let pid: u32 = env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "usage: ox-desk-probe PID"))?;
    let root = Path::new("/proc");
    let (app, direct) = sample(root, pid)?;
    let mut pending: Vec<_> = direct.into_iter().collect();
    let mut seen = HashSet::from([pid]);
    let mut children = Vec::new();
    while let Some(child) = pending.pop() {
        if !seen.insert(child) {
            continue;
        }
        match sample(root, child) {
            Ok((row, more)) => {
                children.push(row);
                pending.extend(more);
            }
            Err(e) if process_gone(&e) => (),
            Err(e) => return Err(e),
        }
    }
    let clock_ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    println!(
        "{}",
        json!({"clock_ticks_per_second":clock_ticks_per_second,"app":app,"children":children})
    );
    Ok(())
}
