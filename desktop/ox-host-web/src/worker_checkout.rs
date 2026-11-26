//! A host-to-host checkout session. Git IDs and archive chunks share the same
//! authenticated connection. The receiver's filesystem paths stay on the receiver.
mod connection;
pub(crate) mod receiver;
mod sender;
pub use sender::transfer;
use serde::{Deserialize, Serialize};

const CHUNK: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Checkout {
    source: String,
    target: String,
    repository: String,
    job: u64,
    /// An immutable source-generated UUID for this job, reused if its reply is lost.
    request_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Open(Checkout),
    Known { commits: Vec<String> },
    Archive { bytes: u64, commit: String },
    Finish { sha256: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "step", rename_all = "snake_case")]
enum Reply {
    Ready {
        target: String,
        source: String,
        existing: Option<Receipt>,
    },
    Known {
        commits: Vec<String>,
    },
    Installed(Delivery),
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Receipt {
    pub commit: String,
    pub archive_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Delivery {
    pub receipt: Receipt,
    /// A retry or concurrent request found this job's previously installed snapshot.
    pub recovered: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paired_hosts;
    use std::{env, fs, path::PathBuf};

    #[test]
    #[ignore = "two isolated hosts through the TLS browser fixture"]
    fn linked_checkout() {
        let state = PathBuf::from(env::var_os("OX_TEST_REMOTE_DIR").unwrap());
        let source = fs::read_to_string(state.join("host-id")).unwrap();
        let hosts = paired_hosts::worker_links(&state.join("worker-links.json")).unwrap();
        let send = |request| {
            transfer(
                &hosts[0],
                source.trim(),
                &state.join("inventory-source/ox/nested"),
                &state,
                42,
                request,
            )
        };
        let request = "ddcba183-5073-4e9a-9825-e8730784ce5e";
        if env::var_os("OX_TEST_DENIED_LINK").is_some() {
            assert!(send(request).is_err());
            return;
        }
        let (first, second) = std::thread::scope(|s| {
            let a = s.spawn(|| send(request).unwrap());
            (send(request).unwrap(), a.join().unwrap())
        });
        assert_ne!(first.recovered, second.recovered);
        assert_eq!(first.receipt, second.receipt);
        let receipt = first.receipt;
        assert!(
            receipt.archive_bytes < 1_000_000,
            "the common 2 MiB base should not cross the link"
        );
        let replay = send(request).unwrap();
        assert!(replay.recovered);
        assert_eq!(replay.receipt, receipt);
        assert!(send("3d89eff6-a436-40a4-a9ad-fa294b50c48d").is_err());
        assert!(
            transfer(
                &hosts[0],
                source.trim(),
                &state.join("another/ox"),
                &state,
                43,
                request
            )
            .is_err()
        );
    }
}
