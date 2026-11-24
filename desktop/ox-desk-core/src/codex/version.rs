//! Verify the version advertised by the actual app-server connection.
pub fn minimum() -> &'static str {
    include_str!("../../../gate.toml")
        .lines()
        .find_map(|line| {
            line.strip_prefix("minimum_version = ")?
                .strip_prefix('"')?
                .strip_suffix('"')
        })
        .expect("Codex version floor in gate.toml")
}
fn number(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.trim_start_matches('v').split(['.', '-', '+']);
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}
pub fn unsupported(user_agent: &str) -> Option<String> {
    let product = user_agent.split_whitespace().next()?;
    let (_, version) = product.rsplit_once('/')?;
    let actual = number(version)?;
    let required = number(minimum())?;
    let prerelease = version.contains('-');
    (actual < required || (actual == required && prerelease)).then(|| {
        let (major, minor, patch) = actual;
        format!(
            "Codex {major}.{minor}.{patch}{} is older than the required {}.",
            if prerelease { " pre-release" } else { "" },
            minimum()
        )
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn floor_comes_from_the_schema_gate() {
        assert!(unsupported(&format!("ox-desktop/{} (Linux)", minimum())).is_none());
        assert!(unsupported("ox-desktop/0.125.0 (Linux)").is_some());
        assert!(unsupported("synthetic").is_none());
    }
}
