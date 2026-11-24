//! Fuzzy ranking is independent of GPUI, the active CLI and its protocol.
pub(super) fn rank(query: &str, title: &str, detail: &str) -> Option<i64> {
    let title = title.to_lowercase();
    let detail = detail.to_lowercase();
    let mut total = 0;
    for word in query.split_whitespace() {
        total += match (score(word, &title), score(word, &detail)) {
            (Some(a), Some(b)) => (a + 100).max(b),
            (Some(a), None) => a + 100,
            (None, Some(b)) => b,
            _ => return None,
        };
    }
    Some(total)
}
pub(super) fn preview_rank(query: &str, preview: &str) -> Option<i64> {
    if query.is_empty() || !preview.to_lowercase().contains(query) {
        return None;
    }
    rank(query, preview, "")
}

fn score(word: &str, text: &str) -> Option<i64> {
    if text == word {
        return Some(1000);
    }
    if let Some(index) = text.find(word) {
        return Some(if index == 0 {
            700
        } else {
            550 - index.min(100) as i64
        });
    }
    let mut chars = text.chars().enumerate();
    let mut previous: Option<usize> = None;
    let mut score = 0;
    for wanted in word.chars() {
        let (i, _) = chars.find(|(_, ch)| *ch == wanted)?;
        if let Some(p) = previous {
            score += if i == p + 1 {
                12
            } else {
                -((i - p).min(50) as i64)
            };
        } else {
            score -= i.min(100) as i64;
        }
        previous = Some(i);
    }
    Some(score)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renamed_thread_remains_searchable_by_first_prompt() {
        assert!(
            preview_rank("copper meadow", "Copper meadow model-switch-window")
                > rank(
                    "copper meadow",
                    "/code-review",
                    "Copper command · model meadow"
                )
        );
        assert_eq!(preview_rank("", "Copper meadow"), None);
    }

    #[test]
    fn ranks_exact_prefix_then_subsequence_with_context() {
        assert!(rank("form", "form", "Thread") > rank("form", "Forms", "Thread"));
        assert!(rank("form", "Forms", "Thread") > rank("form", "first order retry mode", "Thread"));
        assert!(rank("fx api", "Firefox fix", "Project · OX API").is_some());
        assert!(rank("fx api", "Firefox fix", "ChatGPT").is_none());
        assert!(rank("åø", "Ålesund øya", "Project").is_some());
        assert_eq!(rank("zzz", "abc", "def"), None);
    }
}
