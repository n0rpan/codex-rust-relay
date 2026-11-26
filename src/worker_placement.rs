//! Project rules belong to the parent project, including sessions opened in a Git subdirectory.
use std::{
    fs::OpenOptions,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

#[derive(Default)]
pub struct Rule {
    pub host: Option<String>,
    pub os: Option<String>,
}

pub fn rule(project: &Path, name: Option<&str>) -> Result<Rule, String> {
    let root = project
        .ancestors()
        .find(|p| p.join(".git").exists())
        .unwrap_or(project);
    let path = project
        .ancestors()
        .take_while(|p| p.starts_with(root))
        .map(|p| p.join("codeox.toml"))
        .find(|p| p.exists())
        .unwrap_or_else(|| project.join("codeox.toml"));
    let missing = || match name {
        Some(name) if name != "default" => Err(format!(
            "No [workers.placement] rule named {name:?} in {}.",
            path.display()
        )),
        _ => Ok(Rule::default()),
    };
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return missing(),
        Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
    };
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("{} must be a regular file.", path.display()));
    }
    let mut text = String::new();
    file.take(4 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() > 4 * 1024 * 1024 {
        return Err(format!("{} is too large.", path.display()));
    }
    let doc = toml_edit::Document::parse(text)
        .map_err(|_| format!("Invalid TOML in {}.", path.display()))?;
    let Some(rules) = doc.get("workers").and_then(|w| w.get("placement")) else {
        return missing();
    };
    let rules = rules
        .as_table_like()
        .ok_or("[workers.placement] must be a table.")?;
    let setting = |key: &str| {
        rules
            .get(key)
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| format!("workers.placement.{key} must be a nonempty string."))
            })
            .transpose()
    };
    let host = setting(name.unwrap_or("default"))?;
    if name.is_some_and(|n| n != "default") && host.is_none() {
        return missing();
    }
    Ok(Rule {
        host,
        os: setting("os")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn child_directory_uses_nearest_file_inside_its_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let sub = repo.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        std::fs::write(
            tmp.path().join("codeox.toml"),
            "[workers.placement]\ndefault='outer'",
        )
        .unwrap();
        assert!(rule(&sub, None).unwrap().host.is_none());
        std::fs::write(
            repo.join("codeox.toml"),
            "[workers.placement]\ndefault='parent'\nheavy='label:big'\nos='linux'",
        )
        .unwrap();
        assert_eq!(
            rule(&sub, Some("heavy")).unwrap().host.as_deref(),
            Some("label:big")
        );
        assert_eq!(rule(&sub, None).unwrap().os.as_deref(), Some("linux"));
        assert!(rule(&sub, Some("missing")).is_err());
        std::fs::write(
            sub.join("codeox.toml"),
            "[workers.placement]\ndefault='local-build'",
        )
        .unwrap();
        assert_eq!(
            rule(&sub, None).unwrap().host.as_deref(),
            Some("local-build")
        );
    }
}
