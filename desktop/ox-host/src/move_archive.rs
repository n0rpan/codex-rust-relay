//! The destination's portable, paged copy of a moved session's readable history.
use crate::move_ledger::Plan;
use ox_desk_core::host_protocol::{Timeline, TimelineItem};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, Write},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Header {
    plan: Plan,
    items: usize,
}

struct Archive {
    file: BufReader<File>,
    offsets: Vec<u64>,
}

impl Archive {
    /// Capture the complete projection without carrying live approval or runtime state.
    fn write(file: &mut File, header: &Header, timeline: &Timeline) -> io::Result<[u8; 32]> {
        if timeline.detail["ready"] != true
            || timeline.detail["older"] == true
            || timeline.detail["loading"] == true
            || !timeline.detail["active"].is_null()
        {
            return Err(io::Error::other(
                "Wait for the full session history and current turn.",
            ));
        }
        if header.items != timeline.items.len()
            || header.plan.source.native.is_empty()
            || header.plan.source.host == header.plan.target_host
        {
            return Err(io::Error::other("The transcript does not match the move."));
        }
        if file.metadata()?.len() != 0 {
            return Err(io::Error::other(
                "The transfer archive already contains data.",
            ));
        }
        file.rewind()?;
        serde_json::to_writer(&mut *file, header)?;
        file.write_all(b"\n")?;
        for item in &timeline.items {
            serde_json::to_writer(&mut *file, item)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        checksum(file)
    }

    /// Verify the staged bytes and their binding before making the archive readable.
    fn open(mut file: File, header: &Header, digest: [u8; 32]) -> io::Result<Self> {
        if checksum(&mut file)? != digest {
            return Err(io::Error::other(
                "The moved history failed its integrity check.",
            ));
        }
        file.rewind()?;
        let mut file = BufReader::new(file);
        let mut line = String::new();
        file.read_line(&mut line)?;
        if serde_json::from_str::<Header>(&line)? != *header {
            return Err(io::Error::other("The history belongs to a different move."));
        }
        let mut position = line.len() as u64;
        let mut offsets = Vec::new();
        loop {
            line.clear();
            let bytes = file.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            let _: TimelineItem = serde_json::from_str(&line)?;
            offsets.push(position);
            position += bytes as u64;
        }
        if offsets.len() != header.items {
            return Err(io::Error::other("The history transfer is incomplete."));
        }
        Ok(Self { file, offsets })
    }

    /// Return up to `count` items before a zero-based exclusive cursor.
    fn previous(
        &mut self,
        before: Option<usize>,
        count: usize,
    ) -> io::Result<(Vec<TimelineItem>, Option<usize>)> {
        let end = before.unwrap_or(self.offsets.len());
        if end > self.offsets.len() || count == 0 {
            return Err(io::Error::other("Invalid moved history page."));
        }
        let start = end.saturating_sub(count);
        if start == end {
            return Ok((Vec::new(), None));
        }
        self.file.seek(io::SeekFrom::Start(self.offsets[start]))?;
        let mut page = Vec::with_capacity(end - start);
        let mut line = String::new();
        for _ in start..end {
            line.clear();
            self.file.read_line(&mut line)?;
            page.push(serde_json::from_str(&line)?);
        }
        Ok((page, (start > 0).then_some(start)))
    }
}

fn checksum(file: &mut File) -> io::Result<[u8; 32]> {
    file.rewind()?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            return Ok(digest.finalize().into());
        }
        digest.update(&buffer[..bytes]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::move_ledger::{Ledger, Owner, Prepared, Stage};
    use ox_desk_core::host_protocol::Scope;
    use ox_desk_core::runtime::Route;
    use serde_json::json;
    use uuid::Uuid;

    fn fixture() -> (Header, Timeline) {
        let items = (0..6)
            .map(|i| TimelineItem {
                turn: format!("turn-{i}"),
                id: format!("reply-{i}"),
                role: "assistant".into(),
                text: format!("Original reply {i}\n"),
                revision: 1,
                detail: json!({}),
            })
            .collect::<Vec<_>>();
        let header = Header {
            plan: Plan {
                id: Uuid::new_v4(),
                session: Uuid::new_v4(),
                source: Owner {
                    host: Uuid::new_v4(),
                    generation: 0,
                    scope: Scope {
                        project: "/laptop/ox".into(),
                        route: Route::CODEX_SUBSCRIPTION,
                    },
                    native: "source-thread".into(),
                },
                target_host: Uuid::new_v4(),
                target_scope: Scope {
                    project: "/studio/ox".into(),
                    route: Route::CODEX_SUBSCRIPTION,
                },
            },
            items: items.len(),
        };
        (
            header,
            Timeline {
                items,
                detail: json!({"ready":true}),
            },
        )
    }

    #[test]
    fn history_pages_need_only_the_destination_file() {
        let (header, timeline) = fixture();
        let mut file = tempfile::tempfile().unwrap();
        let digest = Archive::write(&mut file, &header, &timeline).unwrap();
        let mut archive = Archive::open(file, &header, digest).unwrap();
        let (last, before) = archive.previous(None, 4).unwrap();
        assert_eq!(last, timeline.items[2..]);
        let (first, before) = archive.previous(before, 4).unwrap();
        assert_eq!(first, timeline.items[..2]);
        assert_eq!(before, None);
    }

    #[test]
    fn unread_history_and_changed_transfer_bindings_do_not_stage() {
        let (mut header, mut timeline) = fixture();
        let mut file = tempfile::tempfile().unwrap();
        timeline.detail["older"] = true.into();
        assert!(Archive::write(&mut file, &header, &timeline).is_err());
        timeline.detail["older"] = false.into();
        let digest = Archive::write(&mut file, &header, &timeline).unwrap();
        assert!(Archive::open(file.try_clone().unwrap(), &header, [0; 32]).is_err());
        header.plan.target_host = Uuid::new_v4();
        assert!(Archive::open(file, &header, digest).is_err());
    }

    #[test]
    fn the_completed_move_retains_full_history_after_source_storage_goes_away() {
        let (header, timeline) = fixture();
        let plan = &header.plan;
        let laptop = tempfile::tempdir().unwrap();
        let studio = tempfile::tempdir().unwrap();
        let mut source = Ledger::open(laptop.path(), plan.source.host).unwrap();
        let mut receiver = Ledger::open(studio.path(), plan.target_host).unwrap();
        source.prepare_source(plan).unwrap();
        let original = laptop.path().join("history");
        let copy = studio.path().join("history");
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&original)
            .unwrap();
        let digest = Archive::write(&mut file, &header, &timeline).unwrap();
        std::fs::copy(original, &copy).unwrap();
        Archive::open(File::open(&copy).unwrap(), &header, digest).unwrap();
        let staged = Prepared {
            plan: plan.clone(),
            stage: Stage {
                digest,
                native: "target-thread".into(),
            },
        };
        receiver
            .prepare_receiver(plan.source.host, &staged)
            .unwrap();
        source.fence_source(plan.target_host, &staged).unwrap();
        source.writer_released(plan).unwrap();
        let proof = source.commit_source(plan).unwrap();
        let activated = receiver
            .activate_receiver(plan.source.host, &proof)
            .unwrap();
        source.confirm_source(plan.target_host, &activated).unwrap();
        drop(source);
        drop(file);
        laptop.close().unwrap();

        let receiver = Ledger::open(studio.path(), plan.target_host).unwrap();
        assert!(receiver.can_write(&plan.target_scope, "target-thread"));
        let mut archive = Archive::open(File::open(&copy).unwrap(), &header, digest).unwrap();
        assert_eq!(
            archive.previous(None, timeline.items.len()).unwrap(),
            (timeline.items, None)
        );
    }
}
