//! Opt-in update of the running executable from stable GitHub Releases.

use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Client, StatusCode, Url};
use ring::digest;
use serde::Deserialize;

use crate::provider::RelayError;

const REPOSITORY: &str = "n0rpan/codex-rust-relay";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const MAX_RELEASE_BYTES: usize = 512 * 1024;
const MAX_BINARY_BYTES: u64 = 64 * 1024 * 1024;
const USAGE: &str = "Usage: ox update [--check]";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    digest: Option<String>,
}

struct Update {
    version: String,
    asset: String,
    size: u64,
    sha256: [u8; 32],
}

struct Installation {
    path: PathBuf,
    file: File,
}

impl Installation {
    fn lock(path: &Path) -> Result<Self, RelayError> {
        let path = path.canonicalize()?;
        let file = File::open(&path)?;
        file.try_lock()
            .map_err(|error| format!("could not lock the installed ox executable: {error}"))?;
        Ok(Self { path, file })
    }
}

pub(crate) async fn run(args: &[OsString]) -> Result<u8, RelayError> {
    let check_only = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        [flag] if flag == "-h" || flag == "--help" => {
            println!("{USAGE}");
            println!("Install the latest stable ox release over the running executable.");
            println!("Use --check to look for a newer release without installing it.");
            return Ok(0);
        }
        _ => return Err(USAGE.into()),
    };

    let target = target_triple(std::env::consts::OS, std::env::consts::ARCH)
        .ok_or("ox releases support Linux and macOS on x86-64 and ARM64")?;
    let installation = if check_only {
        None
    } else {
        Some(Installation::lock(&std::env::current_exe()?)?)
    };
    let client = Client::builder()
        .user_agent(concat!("ox/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !release_host(attempt.url()) {
                attempt.error("unexpected GitHub release redirect")
            } else {
                attempt.follow()
            }
        }))
        .build()?;
    let metadata_url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let Some(update) = latest(&client, &metadata_url, CURRENT, target).await? else {
        println!("ox {CURRENT} is up to date.");
        return Ok(0);
    };
    let Some(installation) = installation else {
        println!(
            "ox {} is available (installed: {CURRENT}). Run ox update to install it.",
            update.version
        );
        return Ok(0);
    };

    let url = format!(
        "https://github.com/{REPOSITORY}/releases/download/v{}/{}",
        update.version, update.asset
    );
    println!("Updating ox {CURRENT} to {}…", update.version);
    install(&client, &url, &update, &installation).await?;
    println!(
        "Updated to ox {}. Running sessions keep their current version until restarted.",
        update.version
    );
    Ok(0)
}

fn target_triple(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

fn release_host(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

async fn latest(
    client: &Client,
    url: &str,
    current: &str,
    target: &str,
) -> Result<Option<Update>, RelayError> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|_| "could not reach GitHub for an ox update")?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::NOT_FOUND => {
            return Err("no stable ox release is available. Preview builds are available from GitHub Actions".into());
        }
        status => return Err(format!("GitHub release check returned HTTP {status}").into()),
    }
    let body = crate::read_upstream(response, MAX_RELEASE_BYTES)
        .await
        .map_err(|_| "GitHub release metadata was too large or unreadable")?;
    let release: Release =
        serde_json::from_slice(&body).map_err(|_| "invalid GitHub release metadata")?;
    select_update(release, current, target)
}

fn select_update(
    release: Release,
    current: &str,
    target: &str,
) -> Result<Option<Update>, RelayError> {
    let version = release
        .tag_name
        .strip_prefix('v')
        .ok_or("invalid ox release tag")?;
    let newer = parse_version(version).ok_or("invalid ox release version")?;
    let installed = parse_version(current).ok_or("this ox build has no stable update version")?;
    if release.draft || release.prerelease {
        return Err("GitHub did not return a stable ox release".into());
    }
    if newer <= installed {
        return Ok(None);
    }

    let name = format!("ox-{version}-{target}");
    let mut matching = release
        .assets
        .into_iter()
        .filter(|asset| asset.name == name);
    let asset = matching
        .next()
        .ok_or_else(|| format!("release v{version} has no ox binary for {target}"))?;
    if matching.next().is_some() || !(1..=MAX_BINARY_BYTES).contains(&asset.size) {
        return Err("invalid ox release asset metadata".into());
    }
    let digest = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .ok_or("GitHub did not provide a SHA-256 digest for this ox binary")?;
    let mut sha256 = [0; 32];
    hex::decode_to_slice(digest, &mut sha256)
        .map_err(|_| "GitHub supplied an invalid SHA-256 digest for this ox binary")?;
    Ok(Some(Update {
        version: version.to_owned(),
        asset: name,
        size: asset.size,
        sha256,
    }))
}

fn parse_version(text: &str) -> Option<[u64; 3]> {
    let mut components = text.split('.');
    let mut result = [0; 3];
    for number in &mut result {
        let part = components.next()?;
        if part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        *number = part.parse().ok()?;
    }
    components.next().is_none().then_some(result)
}

#[cfg(unix)]
async fn install(
    client: &Client,
    url: &str,
    update: &Update,
    installation: &Installation,
) -> Result<(), RelayError> {
    use std::fs::{self, File, OpenOptions, Permissions};
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::PathBuf;

    struct StagingFile(PathBuf);
    impl Drop for StagingFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    let executable = &installation.path;
    let parent = executable
        .parent()
        .ok_or("ox executable has no parent directory")?;
    let metadata = installation.file.metadata()?;

    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| "could not generate an ox update file name")?;
    let stage_path = parent.join(format!(".ox-update-{}", hex::encode(random)));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&stage_path)
        .map_err(|error| format!("could not stage an update in {}: {error}", parent.display()))?;
    let stage = StagingFile(stage_path);
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "could not download the ox release binary from GitHub")?;
    if response.status() != StatusCode::OK {
        return Err(format!("ox binary download returned HTTP {}", response.status()).into());
    }
    let mut sha256 = digest::Context::new(&digest::SHA256);
    let mut received = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "ox binary download was interrupted")?
    {
        received += chunk.len() as u64;
        if received > update.size {
            return Err("ox binary download exceeds its published size".into());
        }
        file.write_all(&chunk)?;
        sha256.update(&chunk);
    }
    if received != update.size || sha256.finish().as_ref() != update.sha256 {
        return Err(
            "downloaded ox binary did not match its published size and SHA-256 digest".into(),
        );
    }
    file.set_permissions(Permissions::from_mode(metadata.mode() & 0o777))?;
    file.sync_all()?;

    let current = fs::symlink_metadata(executable)?;
    if !current.is_file() || current.dev() != metadata.dev() || current.ino() != metadata.ino() {
        return Err(
            "the installed ox executable changed during the update. Run ox update again".into(),
        );
    }
    fs::rename(&stage.0, executable)?;
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(not(unix))]
async fn install(_: &Client, _: &str, _: &Update, _: &Installation) -> Result<(), RelayError> {
    Err("ox updates are supported on Linux and macOS".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tests::TempHome;
    use http_body_util::Full;
    use hyper::Response;
    use hyper::body::Bytes;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::TcpListener;

    const TARGET: &str = "aarch64-apple-darwin";
    const DOWNLOAD: &[u8] = b"new release binary fixture";

    #[test]
    fn compares_numeric_stable_versions_and_limits_redirect_origins() {
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
        for invalid in [
            "1",
            "1.0.0-beta",
            "v1.0.0",
            "1.0.0+extra",
            "01.0.0",
            "1.0.0.0",
        ] {
            assert_eq!(parse_version(invalid), None, "{invalid}");
        }
        for url in [
            "http://github.com/resource",
            "https://github.com:8080/resource",
            "https://github.com.example.org/resource",
            "https://name@release-assets.githubusercontent.com/resource",
        ] {
            assert!(!release_host(&Url::parse(url).unwrap()));
        }
        assert!(release_host(
            &Url::parse("https://release-assets.githubusercontent.com/resource").unwrap()
        ));
    }

    #[tokio::test]
    async fn downloads_and_replaces_only_a_verified_release() {
        let home = TempHome::new();
        let executable = home.0.join("ox");
        fs::write(&executable, "original").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o710)).unwrap();
        let installation = Installation::lock(&executable).unwrap();
        assert!(Installation::lock(&executable).is_err());

        let (base, server) = mock_release(DOWNLOAD).await;
        let client = Client::builder().no_proxy().build().unwrap();
        let metadata = format!("{base}/latest");
        assert!(
            latest(&client, &metadata, "0.2.0", TARGET)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            latest(&client, &metadata, "1.0.0", TARGET)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            latest(&client, &metadata, "0.1.0", "x86_64-apple-darwin")
                .await
                .is_err()
        );
        let update = latest(&client, &metadata, "0.1.0", TARGET)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(update.asset, format!("ox-0.2.0-{TARGET}"));
        install(&client, &format!("{base}/binary"), &update, &installation)
            .await
            .unwrap();
        assert_eq!(fs::read(&executable).unwrap(), DOWNLOAD);
        assert_eq!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
            0o710
        );
        assert_eq!(fs::read_dir(&home.0).unwrap().count(), 1);
        // The old lock cannot replace a file already installed by another writer.
        assert!(
            install(&client, &format!("{base}/binary"), &update, &installation)
                .await
                .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn bad_digest_and_wrong_size_leave_the_installed_file_intact() {
        let home = TempHome::new();
        let executable = home.0.join("ox");
        fs::write(&executable, "original").unwrap();
        let installation = Installation::lock(&executable).unwrap();
        let (base, server) = mock_release(DOWNLOAD).await;
        let client = Client::builder().no_proxy().build().unwrap();
        let mut update = latest(&client, &format!("{base}/latest"), "0.1.0", TARGET)
            .await
            .unwrap()
            .unwrap();

        update.sha256 = [0; 32];
        let url = format!("{base}/binary");
        assert!(
            install(&client, &url, &update, &installation)
                .await
                .unwrap_err()
                .to_string()
                .contains("SHA-256")
        );
        update.size -= 1;
        assert!(
            install(&client, &url, &update, &installation)
                .await
                .unwrap_err()
                .to_string()
                .contains("published size")
        );
        assert_eq!(fs::read(&executable).unwrap(), b"original");
        assert_eq!(fs::read_dir(&home.0).unwrap().count(), 1);
        server.abort();
    }

    async fn mock_release(binary: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let metadata = Bytes::from(json!({
            "tag_name":"v0.2.0",
            "draft": false,
            "prerelease": false,
            "assets": [{
                "name": format!("ox-0.2.0-{TARGET}"),
                "size": binary.len(),
                "digest": format!("sha256:{}", hex::encode(digest::digest(&digest::SHA256, binary)))
            }]
        }).to_string());
        let server = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let metadata = metadata.clone();
                tokio::spawn(async move {
                    let service =
                        service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                            let body = if request.uri().path() == "/latest" {
                                metadata.clone()
                            } else {
                                Bytes::from_static(binary)
                            };
                            async {
                                Ok::<_, std::convert::Infallible>(Response::new(Full::new(body)))
                            }
                        });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        (format!("http://{address}"), server)
    }
}
