//! Deduplicate within a scope and native incarnation. Keep only a digest of
//! potentially large or private action payloads, plus the original disposition.
use ox_desk_core::{
    actions::Action,
    host_protocol::{Scope, Server},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    io,
};

type Key = (Scope, String);
#[derive(Clone)]
pub(super) struct Receipt {
    generation: u64,
    digest: [u8; 32],
    pub(super) result: Server,
    pub(super) opened: Option<String>,
}
#[derive(Default)]
pub(super) struct Receipts {
    entries: HashMap<Key, Receipt>,
    order: VecDeque<Key>,
}
pub(super) fn digest(action: &Action, approval: &Option<String>) -> io::Result<[u8; 32]> {
    let mut hash = Sha256::new();
    serde_json::to_writer(&mut hash, &(action, approval))?;
    Ok(hash.finalize().into())
}
impl Receipts {
    pub fn get(
        &self,
        scope: &Scope,
        id: &str,
        generation: u64,
        digest: [u8; 32],
    ) -> io::Result<Option<&Receipt>> {
        let Some(entry) = self.entries.get(&(scope.clone(), id.to_owned())) else {
            return Ok(None);
        };
        if entry.generation != generation {
            return Err(io::Error::other("Action belongs to an earlier runtime"));
        }
        if entry.digest != digest {
            return Err(io::Error::other(
                "Action ID was reused for a different request",
            ));
        }
        Ok(Some(entry))
    }
    pub fn insert(
        &mut self,
        scope: Scope,
        id: String,
        generation: u64,
        digest: [u8; 32],
        result: Server,
    ) {
        let key = (scope, id);
        self.order.push_back(key.clone());
        self.entries.insert(
            key,
            Receipt {
                generation,
                digest,
                result,
                opened: None,
            },
        );
        while self.order.len() > 2048 {
            self.entries
                .remove(&self.order.pop_front().expect("nonempty receipts"));
        }
    }
    pub fn opened(&mut self, scope: &Scope, key: &str, thread: &str) {
        if let Some(r) = self.entries.get_mut(&(scope.clone(), key.to_owned())) {
            r.opened = Some(thread.to_owned());
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::runtime::Route;
    #[test]
    fn equivalent_retries_share_only_the_original_scope_and_incarnation() {
        let a = Scope {
            project: "/a".into(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let b = Scope {
            project: "/b".into(),
            ..a.clone()
        };
        let hash = digest(&Action::SendText(None, "hello".into()), &None).unwrap();
        let mut r = Receipts::default();
        r.insert(
            a.clone(),
            "x".into(),
            2,
            hash,
            Server::Ack { id: "x".into() },
        );
        r.opened(&a, "x", "thread");
        assert_eq!(
            r.get(&a, "x", 2, hash).unwrap().unwrap().opened.as_deref(),
            Some("thread")
        );
        assert!(r.get(&b, "x", 2, hash).unwrap().is_none());
        assert!(r.get(&a, "x", 3, hash).is_err());
        assert!(
            r.get(
                &a,
                "x",
                2,
                digest(&Action::Stop("id".into()), &None).unwrap()
            )
            .is_err()
        );
    }
}
