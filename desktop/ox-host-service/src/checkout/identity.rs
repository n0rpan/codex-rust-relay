//! A portable identity for matching registered checkouts without disclosing their URLs.
use super::{git, line, run};
use sha2::{Digest, Sha256};
use std::path::Path;
use url::{Host, Url};

pub fn repository_id(project: &Path) -> Option<String> {
    let origin = line(run(git(project).args(["remote", "get-url", "origin"])).ok()?).ok()?;
    identity(origin.to_str()?)
}

pub(super) fn identity(remote: &str) -> Option<String> {
    if remote.chars().any(char::is_control) {
        return None;
    }
    let (url, absolute_scp) = if remote.contains("://") {
        (Url::parse(remote).ok()?, false)
    } else {
        // Colons inside a bracketed IPv6 address belong to the authority.
        let separator = if let Some(close) = remote.find(']') {
            remote[close + 1..].starts_with(':').then_some(close + 1)?
        } else {
            remote.find(':')?
        };
        let (authority, path) = remote.split_at(separator);
        let path = &path[1..];
        if authority.is_empty() || authority.contains(['/', '?', '#']) || path.starts_with(':') {
            return None;
        }
        let mut url = Url::parse(&format!("ssh://{authority}/")).ok()?;
        // Unlike HTTP URLs, scp-style paths can literally contain a question mark or hash.
        url.set_path(path.trim_start_matches('/'));
        (url, path.starts_with('/'))
    };
    let default_port = match url.scheme() {
        "https" => 443,
        "http" => 80,
        "ssh" => 22,
        "git" => 9418,
        _ => return None,
    };
    let host = Host::parse(url.host_str()?).ok()?;
    match &host {
        Host::Domain(name)
            if name.trim_end_matches('.') == "localhost"
                || name.trim_end_matches('.').ends_with(".localhost") =>
        {
            return None;
        }
        Host::Ipv4(ip) if ip.is_loopback() || ip.is_unspecified() => return None,
        Host::Ipv6(ip)
            if ip.is_loopback()
                || ip.is_unspecified()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v| v.is_loopback() || v.is_unspecified()) =>
        {
            return None;
        }
        _ => (),
    }
    let port = url.port().filter(|&p| p != default_port);
    let path = url.path().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty() {
        return None;
    }
    // On a generic SSH server a named login can select a different home repository.
    let user = match (url.scheme(), url.username()) {
        ("ssh", name) if !matches!(name, "" | "git") => name,
        _ => "",
    };
    let port = port
        .map(|p| format!("{}:{p}", url.scheme()))
        .unwrap_or_default();
    // GitHub identifies an owner and repository case-insensitively.
    let path = if matches!(&host, Host::Domain(h) if h == "github.com") {
        std::borrow::Cow::Owned(path.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(path)
    };
    // An explicit absolute scp path can name a different checkout from the
    // home-relative namespace used by the usual git@host:owner/repo form.
    let namespace = if absolute_scp {
        "absolute-scp"
    } else {
        "repository"
    };
    let name = format!("ox-repository-v1\0{namespace}\0{host}\0{port}\0{user}\0{path}");
    let mut digest = Sha256::new();
    digest.update(name);
    // Query parameters can address different repositories on one Git server.
    for (key, value) in url.query_pairs() {
        if matches!(key.as_ref(), "access_token" | "token") {
            continue;
        }
        for field in [key, value] {
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field.as_bytes());
        }
    }
    Some(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_origins_match_without_transport_credentials() {
        assert_eq!(
            identity("git@github.com:Owner/Repo.git"),
            identity("https://github.com/owner/repo")
        );
        let id = identity("git@EXAMPLE.test:owner/Ox.git").unwrap();
        for remote in [
            "ssh://git@example.test:22/owner/Ox.git/",
            "https://user:password@example.test:443/owner/Ox?token=secret",
            "git://example.test/owner/Ox.git",
        ] {
            assert_eq!(identity(remote).as_deref(), Some(id.as_str()));
        }
        for remote in [
            "https://example.test/owner/ox",
            "ssh://git@example.test:8022/owner/Ox",
            "https://another.test/owner/Ox",
            "guest@example.test:owner/Ox",
            "git@example.test:/owner/Ox.git",
            "git@example.test:owner/Ox?token=different",
            "https://example.test/owner/Ox?repository=elsewhere",
        ] {
            assert_ne!(identity(remote).as_deref(), Some(id.as_str()));
        }
        for path in [
            "../Ox",
            "/tmp/clone:ox",
            "file:///tmp/Ox",
            "ext::git",
            "http://127.0.0.1/r",
            "ssh://localhost/r",
            "git@127.0.0.1:r",
            "https://[::1]/r",
        ] {
            assert!(identity(path).is_none());
        }
    }
}
