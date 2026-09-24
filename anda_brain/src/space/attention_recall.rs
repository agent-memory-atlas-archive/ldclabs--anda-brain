//! Attention recall (KIP Memory Interface §4), mapped onto this Brain's API.
//!
//! Every item is raised by a commit: a `watch_fire` Activity for a fired
//! Watch, or a `commitment_review` Activity raising one due Commitment. The
//! item's `raised_seq` is that Activity's `space_seq`. Items are ordered by
//! `(raised_seq, ref)` and the cursor is the last delivered position, so a page
//! may stop inside one commit and the next page continues after that item.
//! Reading is read-only: the host keeps the cursor, and consuming an item
//! changes nothing in memory. An item grants nothing; acting on it passes the
//! action gate like any act.
use super::*;
use crate::types::{AttentionRecall, AttentionRecallInput, AttentionRecallItem};
use serde_json::Value as Json;

/// The cursor prefix. `attention:start` is before every item, `attention:<seq>`
/// is after every item raised at `seq`, and `attention:<seq>:<ref>` is after
/// that one item.
const CURSOR_PREFIX: &str = "attention:";

/// The cursor before every item.
const CURSOR_START: &str = "attention:start";

/// The most items one page delivers.
const MAX_PAGE: usize = 50;

/// The most raising Activities one page reads. A single commit that raised
/// more than this is read up to this many Activities.
const ACTIVITY_WINDOW: usize = 200;

/// The most Commitments one `commitment_review` raises into one page.
const MAX_REVIEW_TARGETS: usize = 128;

/// A position in the attention order: `(raised_seq, ref)`, where a missing ref
/// covers the whole `raised_seq`.
#[derive(Debug, Clone, PartialEq)]
struct Position {
    seq: u64,
    reference: Option<String>,
}

impl Position {
    /// Whether an item at `(seq, reference)` comes after this position.
    fn precedes(&self, seq: u64, reference: &str) -> bool {
        seq > self.seq
            || (seq == self.seq
                && self
                    .reference
                    .as_deref()
                    .is_some_and(|after| reference > after))
    }
}

impl Space {
    pub async fn recall_attention(
        &self,
        input: AttentionRecallInput,
    ) -> Result<AttentionRecall, BoxError> {
        let after = match input.attention_cursor.as_deref() {
            None => None,
            Some(cursor) => parse_cursor(cursor)?,
        };
        let limit = input.limit.unwrap_or(20);
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(format!("attention limit must be 1..={MAX_PAGE}").into());
        }
        // A cursor inside a commit reads that commit again and skips what was
        // delivered; a whole-commit cursor starts at the next one.
        let from = match &after {
            None => 0,
            Some(Position {
                seq,
                reference: Some(_),
            }) => *seq,
            Some(Position {
                seq,
                reference: None,
            }) => seq.saturating_add(1),
        };
        let rows = self
            .read_rows(kip::request_with(
                r#"FIND(?a.id, ?a._system.space_seq, ?a.activity_class) WHERE {
  ?a ACTIVITY {}
  FILTER((?a.activity_class == "watch_fire" || ?a.activity_class == "commitment_review") && ?a._system.space_seq >= :from)
} ORDER BY ?a._system.space_seq ASC LIMIT :limit"#,
                serde_json::Map::from_iter([
                    ("from".to_string(), Json::from(from)),
                    ("limit".to_string(), Json::from(ACTIVITY_WINDOW)),
                ]),
            ))
            .await?;
        let mut raised: Vec<(String, u64, String)> = rows
            .iter()
            .filter_map(|row| {
                let columns = row.as_array()?;
                Some((
                    columns.first()?.as_str()?.to_string(),
                    columns.get(1)?.as_u64()?,
                    columns.get(2)?.as_str()?.to_string(),
                ))
            })
            .collect();
        // A full window may have cut its last commit in two; leave that commit
        // to the next page so each commit is read whole.
        let window_full = raised.len() >= ACTIVITY_WINDOW;
        if window_full
            && let (Some(first), Some(last)) = (raised.first(), raised.last())
            && first.1 != last.1
        {
            let last = last.1;
            raised.retain(|(_, seq, _)| *seq != last);
        }

        let mut items: Vec<AttentionRecallItem> = Vec::new();
        let mut more = false;
        let mut scanned_through = None;
        let mut index = 0;
        'commits: while index < raised.len() {
            let seq = raised[index].1;
            let mut commit = Vec::new();
            while index < raised.len() && raised[index].1 == seq {
                let (activity, _, class) = &raised[index];
                commit.extend(self.raised_items(activity, seq, class).await?);
                index += 1;
            }
            commit.sort_by(|a, b| a.reference.cmp(&b.reference));
            commit.dedup_by(|a, b| a.reference == b.reference);
            for item in commit {
                if after
                    .as_ref()
                    .is_some_and(|after| !after.precedes(seq, &item.reference))
                {
                    continue;
                }
                if items.len() == limit {
                    more = true;
                    break 'commits;
                }
                items.push(item);
            }
            scanned_through = Some(seq);
        }

        let attention_cursor = if more {
            let last = items.last().expect("a cut page delivered items");
            format!("{CURSOR_PREFIX}{}:{}", last.raised_seq, last.reference)
        } else if let Some(seq) = scanned_through {
            format!("{CURSOR_PREFIX}{seq}")
        } else {
            input
                .attention_cursor
                .unwrap_or_else(|| CURSOR_START.to_string())
        };
        Ok(AttentionRecall {
            items,
            attention_cursor,
            complete: !more && !window_full,
        })
    }

    /// The items one raising Activity carries.
    async fn raised_items(
        &self,
        activity: &str,
        raised_seq: u64,
        class: &str,
    ) -> Result<Vec<AttentionRecallItem>, BoxError> {
        let inputs = self
            .read_rows(kip::request_with(
                r#"FIND(?x.id, ?x.schema_ref, ?x.attributes) WHERE {
  ?a ACTIVITY {id: :activity}
  STRUCTURAL (?a, "inputs", ?x)
  ?x CONCEPT {}
} LIMIT :limit"#,
                serde_json::Map::from_iter([
                    ("activity".to_string(), Json::from(activity)),
                    ("limit".to_string(), Json::from(MAX_REVIEW_TARGETS)),
                ]),
            ))
            .await?;
        let mut items = Vec::new();
        for input in inputs.iter().filter_map(Json::as_array) {
            let (Some(id), Some(schema_ref)) = (
                input.first().and_then(Json::as_str),
                input.get(1).and_then(Json::as_str),
            ) else {
                continue;
            };
            let attributes = input.get(2).cloned().unwrap_or(Json::Null);
            items.push(match class {
                "watch_fire" if schema_ref == profile!("Watch") => AttentionRecallItem {
                    reference: id.to_string(),
                    kind: "watch_fired".into(),
                    summary: summary(&attributes, "Watch fired"),
                    raised_seq,
                    due_at: None,
                    target_refs: self.watched_targets(id).await?,
                    priority: None,
                },
                "commitment_review" if schema_ref == profile!("Commitment") => {
                    AttentionRecallItem {
                        reference: id.to_string(),
                        kind: "commitment_due".into(),
                        summary: summary(&attributes, "Commitment due"),
                        raised_seq,
                        due_at: attributes["due_at"].as_str().map(str::to_string),
                        target_refs: vec![id.to_string()],
                        priority: attributes["priority"].as_f64(),
                    }
                }
                _ => continue,
            });
        }
        Ok(items)
    }

    /// What a Watch is about: its `watches` targets.
    async fn watched_targets(&self, watch: &str) -> Result<Vec<String>, BoxError> {
        let rows = self
            .read_rows(kip::request_with(
                r#"FIND(?t.id) WHERE {
  ?w CONCEPT {id: :watch}
  STRUCTURAL (?w, "watches", ?t)
} LIMIT :limit"#,
                serde_json::Map::from_iter([
                    ("watch".to_string(), Json::from(watch)),
                    ("limit".to_string(), Json::from(MAX_REVIEW_TARGETS)),
                ]),
            ))
            .await?;
        Ok(rows
            .iter()
            .filter_map(|id| id.as_str().map(str::to_string))
            .collect())
    }

    async fn read_rows(&self, request: Request) -> Result<Vec<Json>, BoxError> {
        let response = self.execute_kip_readonly(request).await?;
        if !kip::succeeded(&response) {
            return Err(kip::error_message(&response).into());
        }
        Ok(kip::ok_result(&response)
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default())
    }
}

/// `attention:start` (or the older `attention:-1`) is before every item;
/// `attention:<seq>` is after every item raised at `seq`; `attention:<seq>:<ref>`
/// is after that one item (Memory Interface §4).
fn parse_cursor(cursor: &str) -> Result<Option<Position>, BoxError> {
    let invalid = || -> BoxError { "invalid attention cursor".into() };
    let value = cursor.strip_prefix(CURSOR_PREFIX).ok_or_else(invalid)?;
    if value == "start" || value == "-1" {
        return Ok(None);
    }
    let (seq, reference) = match value.split_once(':') {
        Some((seq, reference)) if !reference.is_empty() => (seq, Some(reference.to_string())),
        Some(_) => return Err(invalid()),
        None => (value, None),
    };
    let seq = seq.parse::<u64>().map_err(|_| invalid())?;
    Ok(Some(Position { seq, reference }))
}

fn summary(attributes: &Json, fallback: &str) -> String {
    let text: String = attributes["summary"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(4096)
        .collect();
    if text.trim().is_empty() {
        fallback.to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_name_a_commit_or_one_item_within_it() {
        assert_eq!(parse_cursor("attention:start").unwrap(), None);
        assert_eq!(parse_cursor("attention:-1").unwrap(), None);
        assert_eq!(
            parse_cursor("attention:7").unwrap(),
            Some(Position {
                seq: 7,
                reference: None
            })
        );
        assert_eq!(
            parse_cursor("attention:7:C-12").unwrap(),
            Some(Position {
                seq: 7,
                reference: Some("C-12".into())
            })
        );
        for invalid in [
            "7",
            "attention:",
            "attention:x",
            "attention:7:",
            "attention:-2",
        ] {
            assert!(parse_cursor(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn a_position_orders_by_commit_then_reference() {
        let whole = Position {
            seq: 7,
            reference: None,
        };
        assert!(!whole.precedes(7, "C-1"));
        assert!(whole.precedes(8, "C-0"));
        let item = Position {
            seq: 7,
            reference: Some("C-2".into()),
        };
        assert!(!item.precedes(7, "C-1"));
        assert!(!item.precedes(7, "C-2"));
        assert!(item.precedes(7, "C-3"));
        assert!(item.precedes(8, "C-1"));
    }
}
