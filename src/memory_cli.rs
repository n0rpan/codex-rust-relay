//! `ox memory` CLI subcommand for inspecting and managing OX memories.

use crate::provider::RelayError;
use ox_memory::{
    DEFAULT_BRIEF_CHARS, Kind, MemoryStore, NewMemory, RecallQuery, Scope, Sensitivity,
    canonical_project_id, default_host_id,
};
use std::{ffi::OsString, path::Path};

pub(crate) fn run(args: &[OsString]) -> Result<u8, RelayError> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    run_in(args, &cwd, MemoryStore::open_default().map_err(err)?)
}

fn run_in(args: &[OsString], cwd: &Path, store: MemoryStore) -> Result<u8, RelayError> {
    let Some(first) = args.first().and_then(|a| a.to_str()) else {
        print_usage();
        return Ok(0);
    };
    if matches!(first, "-h" | "--help" | "help") {
        print_usage();
        return Ok(0);
    }

    let project = canonical_project_id(cwd);
    let host = default_host_id();
    let rest: Vec<String> = args[1..]
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    match first {
        "list" | "ls" => cmd_list(&store, &rest, &project, &host),
        "search" | "recall" => cmd_search(&store, &rest, &project, &host),
        "add" | "save" => cmd_add(&store, &rest, &project, &host),
        "pin" => cmd_pin(&store, &rest, true, &host),
        "unpin" => cmd_pin(&store, &rest, false, &host),
        "rm" | "forget" | "delete" => cmd_rm(&store, &rest, &host),
        "brief" => cmd_brief(&store, &rest, &project, &host),
        other => Err(RelayError::from(format!(
            "Unknown memory subcommand '{other}'. Run `ox memory --help`."
        ))),
    }
}

fn cmd_list(
    store: &MemoryStore,
    args: &[String],
    project: &str,
    host: &str,
) -> Result<u8, RelayError> {
    let mut scope_filter: Option<Scope> = None;
    let mut kind_filter: Option<Kind> = None;
    let mut max_sensitivity = Sensitivity::LocalOnly;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scope" => {
                let val = next_val(args, &mut i, "--scope")?;
                scope_filter = parse_scope_filter(val, project, host)?;
            }
            "--kind" => {
                let val = next_val(args, &mut i, "--kind")?;
                kind_filter = Some(Kind::parse(val).map_err(err)?);
            }
            "--sensitivity" => {
                let val = next_val(args, &mut i, "--sensitivity")?;
                max_sensitivity = Sensitivity::parse(val).map_err(err)?;
            }
            other => {
                return Err(RelayError::from(format!(
                    "Unexpected argument '{other}' for `ox memory list`."
                )));
            }
        }
        i += 1;
    }

    let items = store
        .list(scope_filter.as_ref(), kind_filter, max_sensitivity)
        .map_err(err)?;
    if items.is_empty() {
        println!("No memories stored.");
        return Ok(0);
    }
    for entry in items {
        let pin = if entry.pin { "*" } else { " " };
        let tags = if entry.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", entry.tags.join(","))
        };
        println!(
            "{pin} {}  {:<14} {:<10} {:<10} {}{}",
            entry.id,
            entry.kind.as_str(),
            entry.sensitivity.as_str(),
            entry.scope.short_label(),
            entry.text,
            tags
        );
    }
    Ok(0)
}

fn cmd_search(
    store: &MemoryStore,
    args: &[String],
    project: &str,
    host: &str,
) -> Result<u8, RelayError> {
    let mut query_parts = Vec::new();
    let mut scope_filter: Option<Scope> = None;
    let mut include_all_scopes = false;
    let mut kind_filter: Option<Kind> = None;
    let mut max_sensitivity = Sensitivity::LocalOnly;
    let mut limit = 8usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scope" => {
                let val = next_val(args, &mut i, "--scope")?;
                include_all_scopes = val.trim().eq_ignore_ascii_case("all");
                scope_filter = parse_scope_filter(val, project, host)?;
            }
            "--kind" => {
                let val = next_val(args, &mut i, "--kind")?;
                kind_filter = Some(Kind::parse(val).map_err(err)?);
            }
            "--sensitivity" => {
                let val = next_val(args, &mut i, "--sensitivity")?;
                max_sensitivity = Sensitivity::parse(val).map_err(err)?;
            }
            "--limit" => {
                let val = next_val(args, &mut i, "--limit")?;
                limit = val
                    .parse::<usize>()
                    .map_err(|_| RelayError::from("`--limit` must be a positive integer."))?;
            }
            other if other.starts_with("--") => {
                return Err(RelayError::from(format!(
                    "Unknown flag '{other}' for `ox memory search`."
                )));
            }
            part => query_parts.push(part.to_string()),
        }
        i += 1;
    }

    let hits = store
        .recall(&RecallQuery {
            text: query_parts.join(" "),
            project: Some(project.to_string()),
            host: Some(host.to_string()),
            scope: scope_filter,
            include_all_scopes,
            kind: kind_filter,
            max_sensitivity,
            limit,
        })
        .map_err(err)?;
    if hits.is_empty() {
        println!("No matching memories.");
        return Ok(0);
    }
    for hit in hits {
        let e = hit.entry;
        let pin = if e.pin { "*" } else { " " };
        println!(
            "{pin} {}  ({:.2}) [{}/{}] {}",
            e.id,
            hit.score,
            e.kind.short_label(),
            e.scope.short_label(),
            e.text
        );
    }
    Ok(0)
}

fn cmd_add(
    store: &MemoryStore,
    args: &[String],
    project: &str,
    host: &str,
) -> Result<u8, RelayError> {
    let mut text_parts = Vec::new();
    let mut scope = Scope::Project(project.to_string());
    let mut kind = Kind::Fact;
    let mut sensitivity = Sensitivity::Private;
    let mut pin = false;
    let mut tags = Vec::new();
    let mut id: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scope" => {
                let val = next_val(args, &mut i, "--scope")?;
                scope = parse_write_scope(val, project, host)?;
            }
            "--kind" => {
                let val = next_val(args, &mut i, "--kind")?;
                kind = Kind::parse(val).map_err(err)?;
            }
            "--sensitivity" => {
                let val = next_val(args, &mut i, "--sensitivity")?;
                sensitivity = Sensitivity::parse(val).map_err(err)?;
            }
            "--pin" => pin = true,
            "--tag" => {
                let val = next_val(args, &mut i, "--tag")?;
                for part in val.split(',') {
                    if !part.trim().is_empty() {
                        tags.push(part.trim().to_string());
                    }
                }
            }
            "--id" => {
                let val = next_val(args, &mut i, "--id")?;
                id = Some(val.to_string());
            }
            other if other.starts_with("--") => {
                return Err(RelayError::from(format!(
                    "Unknown flag '{other}' for `ox memory add`."
                )));
            }
            part => text_parts.push(part.to_string()),
        }
        i += 1;
    }

    let text = text_parts.join(" ");
    let outcome = store
        .upsert(NewMemory {
            id,
            scope,
            kind,
            sensitivity,
            text,
            tags,
            pin,
            source_host: host.to_string(),
        })
        .map_err(err)?;
    let entry = outcome.entry();
    println!(
        "{} {} [{}/{}/{}] {}",
        outcome.action(),
        entry.id,
        entry.kind.as_str(),
        entry.scope.short_label(),
        entry.sensitivity.as_str(),
        entry.text
    );
    Ok(0)
}

fn cmd_pin(store: &MemoryStore, args: &[String], pin: bool, host: &str) -> Result<u8, RelayError> {
    let Some(id) = args.first() else {
        return Err(RelayError::from(format!(
            "Usage: ox memory {} <id>",
            if pin { "pin" } else { "unpin" }
        )));
    };
    let entry = store.set_pin(id, pin, host).map_err(err)?;
    println!(
        "{} {} {}",
        if pin { "pinned" } else { "unpinned" },
        entry.id,
        entry.text
    );
    Ok(0)
}

fn cmd_rm(store: &MemoryStore, args: &[String], host: &str) -> Result<u8, RelayError> {
    let Some(id) = args.first() else {
        return Err(RelayError::from("Usage: ox memory rm <id>"));
    };
    let entry = store.forget(id, host).map_err(err)?;
    println!("removed {} {}", entry.id, entry.text);
    Ok(0)
}

fn cmd_brief(
    store: &MemoryStore,
    args: &[String],
    project: &str,
    host: &str,
) -> Result<u8, RelayError> {
    let mut max_sensitivity = Sensitivity::Private;
    let mut max_chars = DEFAULT_BRIEF_CHARS;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--sensitivity" => {
                let val = next_val(args, &mut i, "--sensitivity")?;
                max_sensitivity = Sensitivity::parse(val).map_err(err)?;
            }
            "--max-chars" => {
                let val = next_val(args, &mut i, "--max-chars")?;
                max_chars = val
                    .parse::<usize>()
                    .map_err(|_| RelayError::from("`--max-chars` must be an integer."))?;
            }
            other => {
                return Err(RelayError::from(format!(
                    "Unknown flag '{other}' for `ox memory brief`."
                )));
            }
        }
        i += 1;
    }

    match store
        .tier1_brief(Some(project), Some(host), max_sensitivity, max_chars)
        .map_err(err)?
    {
        Some(brief) => {
            let chars = brief.chars().count();
            let est_tokens = chars.div_ceil(4);
            println!("{brief}\n\n({chars} chars, ~{est_tokens} tokens)");
        }
        None => println!("No pinned memories for this scope and sensitivity."),
    }
    Ok(0)
}

fn next_val<'a>(args: &'a [String], i: &mut usize, flag: &str) -> Result<&'a str, RelayError> {
    *i += 1;
    args.get(*i)
        .map(String::as_str)
        .ok_or_else(|| RelayError::from(format!("Missing value for {flag}.")))
}

fn parse_scope_filter(raw: &str, project: &str, host: &str) -> Result<Option<Scope>, RelayError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "all" => Ok(None),
        _ => parse_write_scope(raw, project, host).map(Some),
    }
}

fn parse_write_scope(raw: &str, project: &str, host: &str) -> Result<Scope, RelayError> {
    let trimmed = raw.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "global" => Ok(Scope::Global),
        "project" => Ok(Scope::Project(project.to_string())),
        "host" => Ok(Scope::Host(host.to_string())),
        _ => {
            if let Some(val) = trimmed.strip_prefix("project:")
                && !val.trim().is_empty()
            {
                return Ok(Scope::Project(val.trim().to_string()));
            }
            if let Some(val) = trimmed.strip_prefix("host:")
                && !val.trim().is_empty()
            {
                return Ok(Scope::Host(val.trim().to_string()));
            }
            Err(RelayError::from(format!(
                "Unknown scope '{raw}'. Expected global, project, host, project:<id>, or host:<id>."
            )))
        }
    }
}

fn err(error: ox_memory::MemoryError) -> RelayError {
    RelayError::from(error.to_string())
}

fn print_usage() {
    println!(
        "\
Manage OX persistent memories across Codex, Claude, and paired hosts.

Usage:
  ox memory list [--scope all|global|project|host] [--kind routing_policy|preference|fact] [--sensitivity public|private|local_only]
  ox memory search <query> [--scope ...] [--kind ...] [--limit N]
  ox memory add <text> [--scope global|project|host] [--kind routing_policy|preference|fact] [--sensitivity public|private|local_only] [--pin] [--tag TAG]
  ox memory pin <id>
  ox memory unpin <id>
  ox memory rm <id>
  ox memory brief [--sensitivity public|private|local_only] [--max-chars N]"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn cli_add_pin_search_brief_and_rm_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path().join("memory.json"));

        assert_eq!(
            run_in(
                &os(&[
                    "add",
                    "Use Claude subscription for confidential work.",
                    "--scope",
                    "global",
                    "--kind",
                    "routing_policy",
                    "--sensitivity",
                    "private",
                    "--pin",
                    "--tag",
                    "routing,claude"
                ]),
                dir.path(),
                store.clone(),
            )
            .unwrap(),
            0
        );

        let items = store.list(None, None, Sensitivity::LocalOnly).unwrap();
        assert_eq!(items.len(), 1);
        let id = items[0].id.clone();

        assert_eq!(
            run_in(&os(&["search", "confidential"]), dir.path(), store.clone()).unwrap(),
            0
        );
        assert_eq!(
            run_in(&os(&["brief"]), dir.path(), store.clone()).unwrap(),
            0
        );
        assert_eq!(
            run_in(&os(&["unpin", &id]), dir.path(), store.clone()).unwrap(),
            0
        );
        assert!(!store.list(None, None, Sensitivity::LocalOnly).unwrap()[0].pin);

        assert_eq!(
            run_in(&os(&["rm", &id]), dir.path(), store.clone()).unwrap(),
            0
        );
        assert!(
            store
                .list(None, None, Sensitivity::LocalOnly)
                .unwrap()
                .is_empty()
        );
    }
}
