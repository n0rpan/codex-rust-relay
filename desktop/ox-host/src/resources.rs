//! Sample host-wide resources once per interval, shared by all connected clients.
use ox_desk_core::host_protocol::HostResources;
use std::time::{Duration, Instant};
use sysinfo::{MemoryRefreshKind, System};

pub(crate) const INTERVAL: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct Resources {
    cached: Option<(Instant, HostResources)>,
    gpus: std::sync::OnceLock<Vec<String>>,
}
impl Resources {
    pub fn next_sample(&self) -> Option<Instant> {
        self.cached.as_ref().map(|(at, _)| *at + INTERVAL)
    }
    pub fn sample(&mut self, running_workers: usize) -> HostResources {
        let now = Instant::now();
        if self.next_sample().is_none_or(|next| now >= next) {
            let mut system = System::new();
            system.refresh_memory_specifics(MemoryRefreshKind::new().with_ram());
            self.cached = Some((
                now,
                HostResources {
                    os: std::env::consts::OS.into(),
                    arch: std::env::consts::ARCH.into(),
                    gpus: self.gpus.get_or_init(gpus).clone(),
                    cpus: std::thread::available_parallelism().ok().map(usize::from),
                    total_memory: system.total_memory(),
                    available_memory: system.available_memory(),
                    load_one_minute: System::load_average().one,
                    running_workers,
                },
            ));
        }
        let (_, cached) = self.cached.as_mut().expect("sampled host");
        cached.running_workers = running_workers;
        cached.clone()
    }
}

#[cfg(target_os = "linux")]
fn gpus() -> Vec<String> {
    // PCI display controllers are visible even on a headless host without X or DRM nodes.
    let mut gpus = Vec::new();
    for pci in std::fs::read_dir("/sys/bus/pci/devices")
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = pci.path();
        let read = |name: &str| {
            std::fs::read_to_string(path.join(name))
                .ok()
                .map(|s| s.trim().to_owned())
        };
        if !read("class").is_some_and(|c| c.starts_with("0x03")) {
            continue;
        }
        if let (Some(vendor), Some(device)) = (read("vendor"), read("device")) {
            let brand = match vendor.as_str() {
                "0x10de" => "NVIDIA",
                "0x1002" => "AMD",
                "0x8086" => "Intel",
                other => other,
            };
            gpus.push(format!("{brand} {}", device.trim_start_matches("0x")));
        }
    }
    gpus.sort();
    gpus
}
#[cfg(target_os = "macos")]
fn gpus() -> Vec<String> {
    use objc2_metal::{MTLCopyAllDevices, MTLDevice};
    let devices = MTLCopyAllDevices();
    (0..devices.len())
        .map(|i| devices.objectAtIndex(i).name().to_string())
        .collect()
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn gpus() -> Vec<String> {
    Vec::new()
}
