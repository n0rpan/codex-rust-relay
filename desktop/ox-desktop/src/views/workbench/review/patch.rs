//! Bound layout work even for generated single-line patches. Copy uses the original data.
use ox_desk_core::transcript::item::FileChange;

#[derive(Clone, Copy, Default)]
pub(super) struct Cursor {
    file: usize,
    byte: usize,
}

pub(super) struct Page {
    pub lines: Vec<String>,
    pub next: Option<Cursor>,
}

pub(super) fn page(patches: &[&FileChange], mut at: Cursor) -> Page {
    const LINE_LIMIT: usize = 64;
    const BYTES_PER_LINE: usize = 512;
    let mut lines = Vec::new();
    if patches
        .get(at.file)
        .is_none_or(|patch| at.byte > patch.diff.len() || !patch.diff.is_char_boundary(at.byte))
    {
        at = Cursor::default();
    }
    while let Some(patch) = patches.get(at.file) {
        if at.byte == patch.diff.len() {
            at = Cursor {
                file: at.file + 1,
                byte: 0,
            };
            continue;
        }
        if lines.len() == LINE_LIMIT {
            return Page {
                lines,
                next: Some(at),
            };
        }
        let remaining = &patch.diff[at.byte..];
        let bounded =
            &remaining[..remaining.floor_char_boundary(remaining.len().min(BYTES_PER_LINE))];
        let chunk = bounded.split_inclusive('\n').next().unwrap();
        lines.push(chunk.trim_end_matches(['\r', '\n']).to_owned());
        at.byte += chunk.len();
    }
    Page { lines, next: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_unicode_lines_and_all_subsequent_edits_stay_reachable() {
        let text = "🌍".repeat(12000);
        let first = FileChange {
            path: "f".into(),
            diff: text.clone().into(),
        };
        let second = FileChange {
            path: "f".into(),
            diff: "last line".into(),
        };
        let files = [&first, &second];
        let mut cursor = Cursor::default();
        let mut displayed = String::new();
        let mut pages = 0;
        loop {
            let page = page(&files, cursor);
            assert!(page.lines.len() <= 64 && page.lines.iter().all(|s| s.len() <= 512));
            displayed.extend(page.lines);
            pages += 1;
            let Some(next) = page.next else { break };
            cursor = next;
        }
        assert!(pages > 1);
        assert_eq!(displayed, format!("{text}last line"));
    }
}
