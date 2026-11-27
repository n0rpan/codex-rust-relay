//! A host's starting mode. The default preserves the operator's interface settings.
use crate::{
    modes::{Decision, Level, ModeRequest, Resolved},
    runtime::Runtime,
};
use serde::{Deserialize, Serialize};
use std::{fs, io, os::unix::fs::DirBuilderExt, path::Path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum HostPolicy {
    #[default]
    FollowInterface,
    AlwaysRead,
    AlwaysWrite,
    AlwaysClassifier,
    AlwaysYolo,
    YoloAsksEveryTime,
}

impl HostPolicy {
    pub const ALL: [Self; 6] = [
        Self::FollowInterface,
        Self::AlwaysRead,
        Self::AlwaysWrite,
        Self::AlwaysClassifier,
        Self::AlwaysYolo,
        Self::YoloAsksEveryTime,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::FollowInterface => "Follow interface",
            Self::AlwaysRead => "Always read",
            Self::AlwaysWrite => "Always write",
            Self::AlwaysClassifier => "Always auto-review",
            Self::AlwaysYolo => "Always YOLO",
            Self::YoloAsksEveryTime => "YOLO asks every time",
        }
    }
    pub fn parse(input: &str) -> io::Result<Self> {
        let name = input.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        let name = match name.as_str() {
            "follow" => "follow_interface",
            "always_auto_review" | "always_auto" => "always_classifier",
            "always_workspace" | "always_accept_edits" => "always_write",
            other => other,
        };
        serde_json::from_value(serde_json::Value::String(name.into())).map_err(|_| io::Error::other(
            "Choose follow-interface, always-read, always-write, always-classifier, always-yolo or yolo-asks-every-time."
        ))
    }
    pub fn save(self, dir: &Path) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(temp.as_file_mut(), &self)?;
        temp.as_file().sync_all()?;
        temp.persist(dir.join("host-mode-policy.json"))?;
        fs::File::open(dir)?.sync_all()
    }

    pub fn read(dir: &Path) -> io::Result<Self> {
        match fs::File::open(dir.join("host-mode-policy.json")) {
            Ok(file) => Ok(serde_json::from_reader(file)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn starting_level(self) -> Option<Level> {
        match self {
            Self::FollowInterface | Self::YoloAsksEveryTime => None,
            Self::AlwaysRead => Some(Level::Read),
            Self::AlwaysWrite => Some(Level::Write),
            Self::AlwaysClassifier => Some(Level::Classifier),
            Self::AlwaysYolo => Some(Level::Yolo),
        }
    }

    /// The caller's authority has already been checked. A policy can set the
    /// starting mode within that authority, and an explicit lower request stands.
    pub fn child_start(
        self,
        original: Resolved,
        request: ModeRequest,
        ceiling: Level,
        runtime: Runtime,
    ) -> Resolved {
        let Some(start) = self.starting_level() else {
            return original;
        };
        let start = start.min(ceiling);
        if matches!(request, ModeRequest::Level(level) if level < start) {
            return original;
        }
        Resolved {
            level: start,
            access: start.preset(runtime),
            decision: Decision::HostPolicy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes;

    #[test]
    fn a_host_start_respects_both_the_stamp_and_an_agents_lower_request() {
        let rt = Runtime::Claude;
        for ceiling in [Level::Read, Level::Write, Level::Classifier, Level::Yolo] {
            for req in [ModeRequest::Level(Level::Read), ModeRequest::Inherit] {
                let original = modes::resolve(Some(ceiling), req, rt).unwrap();
                let start = HostPolicy::AlwaysYolo.child_start(original, req, ceiling, rt);
                assert_eq!(start.level, original.level);
                assert_eq!(start.access, start.level.preset(rt));
                if req == ModeRequest::Inherit {
                    assert_eq!(start.decision, Decision::HostPolicy);
                }
                assert_eq!(
                    HostPolicy::FollowInterface.child_start(original, req, ceiling, rt),
                    original
                );
            }
        }
        let req = ModeRequest::Inherit;
        let original = modes::resolve(Some(Level::Yolo), req, rt).unwrap();
        assert_eq!(
            HostPolicy::AlwaysWrite
                .child_start(original, req, Level::Yolo, rt)
                .level,
            Level::Write
        );
    }

    #[test]
    fn only_a_missing_policy_defaults_to_follow_interface() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            HostPolicy::read(dir.path()).unwrap(),
            HostPolicy::FollowInterface
        );
        fs::write(dir.path().join("host-mode-policy.json"), "broken").unwrap();
        assert!(HostPolicy::read(dir.path()).is_err());
    }
}
