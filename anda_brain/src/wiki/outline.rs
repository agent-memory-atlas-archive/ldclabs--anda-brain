//! Document structure, independent of retrieval packing and chunk limits.

use super::WikiTocEntry;
use super::chunk::{FenceTracker, parse_heading, slugify};
use std::collections::BTreeMap;

/// Each heading covers its own text and descendants until the next heading
/// at the same or a higher level. Anchors depend on heading names, not chunks.
pub(super) fn outline(content: &str) -> Vec<WikiTocEntry> {
    let mut entries: Vec<(usize, WikiTocEntry)> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    let mut fences = FenceTracker::default();
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        if !fences.feed(line)
            && let Some((level, title)) = parse_heading(line.trim())
        {
            for (old_level, entry) in entries.iter_mut().rev() {
                if *old_level >= level && entry.byte_end == content.len() as u64 {
                    entry.byte_end = offset as u64;
                }
            }
            while stack
                .last()
                .is_some_and(|(old_level, _)| *old_level >= level)
            {
                stack.pop();
            }
            stack.push((level, title.clone()));
            let base = slugify(&title);
            let count = names.entry(base.clone()).or_default();
            *count += 1;
            let mut anchor = if *count == 1 {
                base.clone()
            } else {
                format!("{base}-{}", *count)
            };
            // A literal heading named "setup-2" must not collide with a
            // duplicate heading named "setup".
            while entries.iter().any(|(_, e)| e.anchor == anchor) {
                *count += 1;
                anchor = format!("{base}-{}", *count);
            }
            entries.push((
                level,
                WikiTocEntry {
                    anchor,
                    heading_path: stack.iter().map(|(_, title)| title.clone()).collect(),
                    byte_start: offset as u64,
                    byte_end: content.len() as u64,
                },
            ));
        }
        offset += line.len();
    }
    let preamble_end = entries
        .first()
        .map_or(content.len() as u64, |(_, e)| e.byte_start);
    if preamble_end > 0 {
        let mut anchor = "preamble".to_string();
        while entries.iter().any(|(_, e)| e.anchor == anchor) {
            anchor.push('-');
        }
        entries.insert(
            0,
            (
                0,
                WikiTocEntry {
                    anchor,
                    heading_path: Vec::new(),
                    byte_start: 0,
                    byte_end: preamble_end,
                },
            ),
        );
    }
    entries.into_iter().map(|(_, entry)| entry).collect()
}

/// A hit navigates to the deepest real section covering its complete retrieval
/// slice. Retrieval packing may merge short sibling sections under their
/// common parent, so anchoring only the first byte can point at a child whose
/// section ends before the matching text. Pathological cross-root chunks have
/// no covering section and fall back to the section containing their start.
pub(super) fn anchor_covering(entries: &[WikiTocEntry], start: usize, end: usize) -> String {
    let start = start as u64;
    let end = end as u64;
    entries
        .iter()
        .rev()
        .find(|entry| entry.byte_start <= start && entry.byte_end >= end)
        .or_else(|| {
            entries
                .iter()
                .rev()
                .find(|entry| entry.byte_start <= start && entry.byte_end > start)
        })
        .map(|entry| entry.anchor.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_keep_hierarchy_ranges_and_unique_anchors() {
        let content = "# Guide\nintro\n### Setup\na\n### Setup\nb\n## Setup-2\nc\n#### Detail\nd\n";
        let entries = outline(content);
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].byte_end, content.len() as u64);
        assert_eq!(entries[1].heading_path, ["Guide", "Setup"]);
        assert_eq!(entries[2].heading_path, ["Guide", "Setup"]);
        assert_eq!(entries[1].byte_end, entries[2].byte_start);
        let anchors: std::collections::BTreeSet<_> = entries.iter().map(|e| &e.anchor).collect();
        assert_eq!(anchors.len(), entries.len());
        assert_eq!(entries[4].heading_path, ["Guide", "Setup-2", "Detail"]);
    }

    #[test]
    fn fenced_headings_are_not_sections() {
        let content = "intro\n```md\n# sample\n```\n## Real\ntext\n";
        let entries = outline(content);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].heading_path.is_empty());
        assert_eq!(entries[1].heading_path, ["Real"]);
    }
}
