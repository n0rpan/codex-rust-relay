//! The macOS release identity.
//!
//! Releases are signed with OX's Developer ID. A build that carries that
//! signature accepts only updates that carry it too, so a replaced download
//! and checksum cannot install code from anyone else. Local, pull request and
//! nightly builds are signed ad hoc and keep accepting ad hoc packages.
//! `OX_ALLOW_UNSIGNED=1` turns the rule off for whoever wants another build.
use super::*;

/// Developer ID issued by Apple to the OX release team, as a code requirement.
pub(super) const REQUIREMENT: &str = "=anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"ZN9XKLHU6J\"";

fn satisfies(code: &Path) -> bool {
    std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", "-R", REQUIREMENT])
        .arg(code)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Whether the running executable is a signed release.
pub(super) fn running_release() -> bool {
    std::env::current_exe().is_ok_and(|exe| satisfies(&exe))
}

/// Refuse `code` unless every item has the release identity, when the
/// installation being updated has it. `what` names the update in the refusal.
pub(super) fn require(release: bool, what: &str, code: &[&Path]) -> Result<(), RelayError> {
    if release && !allowed_unsigned() && !code.iter().all(|item| satisfies(item)) {
        return Err(format!(
            "{what} is not signed by the OX release identity. The update was not installed. \
             To install it anyway, set {ALLOW_UNSIGNED}=1."
        )
        .into());
    }
    Ok(())
}

/// The owner of the machine decides: with this set, a signed installation
/// accepts any build, for example a local or pull request build.
const ALLOW_UNSIGNED: &str = "OX_ALLOW_UNSIGNED";

fn allowed_unsigned() -> bool {
    std::env::var_os(ALLOW_UNSIGNED).is_some_and(|value| value == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_code_is_refused_only_for_a_signed_installation() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(require(false, "OX 2.0.0", &[file.path()]).is_ok());
        let refused = require(true, "OX 2.0.0", &[file.path()]).unwrap_err();
        assert!(refused.to_string().contains("release identity"));
        assert!(require(true, "OX 2.0.0", &[]).is_ok());
    }
}
