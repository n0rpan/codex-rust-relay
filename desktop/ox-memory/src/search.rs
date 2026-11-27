use crate::{Kind, MemoryEntry, Scope, Sensitivity};
use std::collections::{HashMap, HashSet};

/// Default character budget for Tier-1 memory brief (~220 tokens).
pub const DEFAULT_BRIEF_CHARS: usize = 900;

/// Query parameters for recalling memories.
#[derive(Clone, Debug)]
pub struct RecallQuery {
    /// Free-text search query. Empty string returns highest-priority entries in scope.
    pub text: String,
    /// Active canonical project ID (used for scope filtering & affinity boost).
    pub project: Option<String>,
    /// Active host ID (used for scope filtering & affinity boost).
    pub host: Option<String>,
    /// Optional explicit scope filter (`None` matches Global + active project + active host).
    pub scope: Option<Scope>,
    /// Optional kind filter.
    pub kind: Option<Kind>,
    /// Maximum allowed sensitivity level.
    pub max_sensitivity: Sensitivity,
    /// Maximum number of hits to return.
    pub limit: usize,
}

impl Default for RecallQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            project: None,
            host: None,
            scope: None,
            kind: None,
            max_sensitivity: Sensitivity::Private,
            limit: 6,
        }
    }
}

/// Ranked recall hit.
#[derive(Clone, Debug)]
pub struct RecallHit {
    pub entry: MemoryEntry,
    pub score: f64,
}

/// Rank active memories matching `query` using BM25 + scope/kind/pin/recency signals.
pub fn rank_memories(entries: &[MemoryEntry], query: &RecallQuery) -> Vec<RecallHit> {
    if query.limit == 0 {
        return Vec::new();
    }

    let candidates: Vec<&MemoryEntry> = entries
        .iter()
        .filter(|e| !e.deleted)
        .filter(|e| e.sensitivity <= query.max_sensitivity)
        .filter(|e| query.kind.is_none_or(|k| e.kind == k))
        .filter(|e| scope_matches(e, query))
        .collect();

    if candidates.is_empty() {
        return Vec::new();
    }

    let query_tokens = tokenize(&query.text);
    let routing_intent = query_tokens.iter().any(|t| {
        matches!(
            t.as_str(),
            "provider"
                | "providers"
                | "model"
                | "models"
                | "billing"
                | "subscription"
                | "api"
                | "route"
                | "routing"
                | "private"
                | "public"
                | "data"
                | "openrouter"
                | "claude"
                | "codex"
                | "openai"
                | "anthropic"
                | "local"
        )
    });

    if query_tokens.is_empty() {
        let mut hits: Vec<RecallHit> = candidates
            .into_iter()
            .map(|entry| {
                let score = prior_score(entry, query, false);
                RecallHit {
                    entry: entry.clone(),
                    score,
                }
            })
            .collect();
        sort_hits(&mut hits);
        hits.truncate(query.limit);
        return hits;
    }

    // Build tokenized documents (tags weighted 2x by repeating them once).
    let docs: Vec<Vec<String>> = candidates
        .iter()
        .map(|entry| {
            let mut toks = tokenize(&entry.text);
            for tag in &entry.tags {
                let tag_toks = tokenize(tag);
                toks.extend(tag_toks.iter().cloned());
                toks.extend(tag_toks);
            }
            toks.extend(tokenize(entry.kind.as_str()));
            toks
        })
        .collect();

    let n = docs.len() as f64;
    let avg_dl = (docs.iter().map(Vec::len).sum::<usize>() as f64 / n).max(1.0);

    let mut df: HashMap<&str, usize> = HashMap::new();
    for doc in &docs {
        let unique: HashSet<&str> = doc.iter().map(String::as_str).collect();
        for tok in unique {
            *df.entry(tok).or_insert(0) += 1;
        }
    }

    let k1 = 1.2_f64;
    let b = 0.75_f64;
    let unique_query: Vec<&str> = {
        let mut seen = HashSet::new();
        query_tokens
            .iter()
            .map(String::as_str)
            .filter(|t| seen.insert(*t))
            .collect()
    };

    let mut hits = Vec::new();
    for (entry, doc) in candidates.into_iter().zip(docs.iter()) {
        let dl = doc.len() as f64;
        let mut tf_map: HashMap<&str, usize> = HashMap::new();
        for tok in doc {
            *tf_map.entry(tok.as_str()).or_insert(0) += 1;
        }

        let mut bm25 = 0.0_f64;
        for &qtok in &unique_query {
            let exact_tf = *tf_map.get(qtok).unwrap_or(&0) as f64;
            let tf = if exact_tf > 0.0 {
                exact_tf
            } else if qtok.len() >= 4 {
                // Lightweight prefix match (e.g. "provider" matches "providers").
                tf_map
                    .iter()
                    .filter(|(k, _)| k.starts_with(qtok) || (k.len() >= 4 && qtok.starts_with(*k)))
                    .map(|(_, &c)| c as f64 * 0.6)
                    .sum()
            } else {
                0.0
            };
            if tf <= 0.0 {
                continue;
            }
            let doc_freq = *df.get(qtok).unwrap_or(&1) as f64;
            let idf = ((n - doc_freq + 0.5) / (doc_freq + 0.5) + 1.0).ln();
            bm25 += idf * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b + b * dl / avg_dl));
        }

        if bm25 <= 0.0 {
            continue;
        }

        let score = bm25 + prior_score(entry, query, routing_intent);
        hits.push(RecallHit {
            entry: entry.clone(),
            score,
        });
    }

    sort_hits(&mut hits);
    hits.truncate(query.limit);
    hits
}

/// Format a deterministic, strictly budgeted Tier-1 brief of pinned memories.
pub fn format_tier1_brief(
    entries: &[MemoryEntry],
    project: Option<&str>,
    host: Option<&str>,
    max_sensitivity: Sensitivity,
    max_chars: usize,
) -> Option<String> {
    if max_chars < 40 {
        return None;
    }

    let mut pinned: Vec<&MemoryEntry> = entries
        .iter()
        .filter(|e| !e.deleted && e.pin && e.sensitivity <= max_sensitivity)
        .filter(|e| match &e.scope {
            Scope::Global => true,
            Scope::Project(p) => project.is_some_and(|cur| cur == p),
            Scope::Host(h) => host.is_some_and(|cur| cur == h),
        })
        .collect();

    if pinned.is_empty() {
        return None;
    }

    pinned.sort_by(|a, b| {
        kind_rank(a.kind)
            .cmp(&kind_rank(b.kind))
            .then_with(|| scope_rank(&a.scope).cmp(&scope_rank(&b.scope)))
            .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
            .then_with(|| a.id.cmp(&b.id))
    });

    let header = "OX memory (pinned):";
    let mut out = String::from(header);
    let mut added = 0usize;

    for entry in pinned {
        let line = format!(
            "\n- [{}/{}] {}",
            entry.kind.short_label(),
            entry.scope.short_label(),
            entry.text
        );
        if out.chars().count() + line.chars().count() > max_chars {
            if added == 0 {
                // Always include at least one truncated line if a single pinned entry is huge.
                let avail = max_chars.saturating_sub(out.chars().count() + 1);
                if avail > 12 {
                    let truncated: String = line.chars().take(avail).collect();
                    out.push_str(&truncated);
                    out.push('…');
                    added += 1;
                }
            }
            break;
        }
        out.push_str(&line);
        added += 1;
    }

    (added > 0).then_some(out)
}

fn kind_rank(kind: Kind) -> u8 {
    match kind {
        Kind::RoutingPolicy => 0,
        Kind::Preference => 1,
        Kind::Fact => 2,
    }
}

fn scope_rank(scope: &Scope) -> u8 {
    match scope {
        Scope::Project(_) => 0,
        Scope::Host(_) => 1,
        Scope::Global => 2,
    }
}

fn scope_matches(entry: &MemoryEntry, query: &RecallQuery) -> bool {
    if let Some(explicit) = &query.scope {
        return &entry.scope == explicit;
    }
    match &entry.scope {
        Scope::Global => true,
        Scope::Project(p) => query.project.as_deref() == Some(p.as_str()),
        Scope::Host(h) => query.host.as_deref() == Some(h.as_str()),
    }
}

fn prior_score(entry: &MemoryEntry, query: &RecallQuery, routing_intent: bool) -> f64 {
    let mut score = 0.0;
    match &entry.scope {
        Scope::Project(p) if query.project.as_deref() == Some(p.as_str()) => score += 0.45,
        Scope::Host(h) if query.host.as_deref() == Some(h.as_str()) => score += 0.30,
        Scope::Global => score += 0.15,
        _ => {}
    }
    if entry.pin {
        score += 0.35;
    }
    if routing_intent && entry.kind == Kind::RoutingPolicy {
        score += 0.50;
    }
    score
}

fn sort_hits(hits: &mut [RecallHit]) {
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.entry.updated_at_ms.cmp(&a.entry.updated_at_ms))
            .then_with(|| a.entry.id.cmp(&b.entry.id))
    });
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .map(|part| part.trim_matches(|c| c == '_' || c == '-'))
        .filter(|part| part.len() >= 2)
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        id: &str,
        scope: Scope,
        kind: Kind,
        sensitivity: Sensitivity,
        pin: bool,
        text: &str,
        tags: &[&str],
    ) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            scope,
            kind,
            sensitivity,
            text: text.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            pin,
            created_at_ms: 100,
            updated_at_ms: 100,
            source_host: "yoga".to_string(),
            seq: 0,
            deleted: false,
        }
    }

    #[test]
    fn bm25_ranks_routing_policy_and_respects_sensitivity() {
        let entries = vec![
            entry(
                "000000000001",
                Scope::Global,
                Kind::RoutingPolicy,
                Sensitivity::Public,
                true,
                "Use OpenRouter fast models for public web research.",
                &["routing", "openrouter"],
            ),
            entry(
                "000000000002",
                Scope::Global,
                Kind::RoutingPolicy,
                Sensitivity::Private,
                true,
                "Use Claude subscription for confidential banking work.",
                &["routing", "claude", "private"],
            ),
            entry(
                "000000000003",
                Scope::Project("github.com/oxstack/ox".into()),
                Kind::Fact,
                Sensitivity::Private,
                false,
                "Run cargo clippy --locked --all-targets -- -D warnings before PRs.",
                &["ci", "rust"],
            ),
        ];

        // Public ceiling excludes Private entry
        let hits = rank_memories(
            &entries,
            &RecallQuery {
                text: "which provider for confidential banking".into(),
                project: Some("github.com/oxstack/ox".into()),
                max_sensitivity: Sensitivity::Public,
                ..Default::default()
            },
        );
        assert!(
            hits.iter()
                .all(|h| h.entry.sensitivity == Sensitivity::Public)
        );

        // Private ceiling ranks Claude subscription policy #1
        let hits = rank_memories(
            &entries,
            &RecallQuery {
                text: "which provider for confidential banking".into(),
                project: Some("github.com/oxstack/ox".into()),
                max_sensitivity: Sensitivity::Private,
                ..Default::default()
            },
        );
        assert_eq!(hits[0].entry.id, "000000000002");
    }

    #[test]
    fn tier1_brief_orders_policies_first_and_enforces_budget() {
        let entries = vec![
            entry(
                "000000000001",
                Scope::Global,
                Kind::Fact,
                Sensitivity::Public,
                true,
                "General pinned fact.",
                &[],
            ),
            entry(
                "000000000002",
                Scope::Global,
                Kind::RoutingPolicy,
                Sensitivity::Public,
                true,
                "Use Claude subscription for private data.",
                &[],
            ),
            entry(
                "000000000003",
                Scope::Global,
                Kind::Preference,
                Sensitivity::LocalOnly,
                true,
                "Local-only secret preference.",
                &[],
            ),
        ];

        let brief = format_tier1_brief(&entries, None, None, Sensitivity::Private, 200).unwrap();
        assert!(brief.len() <= 200);
        assert!(!brief.contains("Local-only"));
        let policy_pos = brief.find("[policy/global]").unwrap();
        let fact_pos = brief.find("[fact/global]").unwrap();
        assert!(policy_pos < fact_pos);
    }
}
