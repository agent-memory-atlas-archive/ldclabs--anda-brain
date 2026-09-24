//! Attention recall (KIP Memory Interface §4), mapped onto this Brain's API.
//!
//! Every item is raised by a commit: a `watch_fire` Activity for a fired
//! Watch, or a `commitment_review` Activity in which Maintenance recorded the
//! Commitments it found due. The item's `raised_seq` is that Activity's
//! `space_seq`, which is what the cursor orders by. Reading is read-only: the
//! host keeps the cursor, and consuming an item changes nothing in memory.
//! An item grants nothing; acting on it passes the action gate like any act.
use super::*;
use crate::types::{AttentionRecall, AttentionRecallInput, AttentionRecallItem};
use serde_json::Value as Json;

/// The cursor prefix; the number after it is the highest delivered `raised_seq`.
const CURSOR_PREFIX: &str = "attention:";

/// The most raising Activities one page reads.
const MAX_PAGE: usize = 50;

/// The most Commitments one `commitment_review` raises into one page.
const MAX_REVIEW_TARGETS: usize = 128;

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
        let rows = self
            .read_rows(kip::request_with(
                r#"FIND(?a.id, ?a._system.space_seq, ?a.activity_class) WHERE {
  ?a ACTIVITY {}
  FILTER((?a.activity_class == "watch_fire" || ?a.activity_class == "commitment_review") && ?a._system.space_seq > :after)
} ORDER BY ?a._system.space_seq ASC LIMIT :limit"#,
                serde_json::Map::from_iter([
                    ("after".to_string(), Json::from(after.map_or(-1, |seq| seq as i64))),
                    ("limit".to_string(), Json::from(limit)),
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
        // A full page may have cut one commit's Activities in two. Stop one
        // coordinate short so the next page reads that commit whole; a single
        // commit larger than the page is delivered as it is.
        let complete = raised.len() < limit;
        if !complete
            && let (Some(first), Some(last)) = (raised.first(), raised.last())
            && first.1 != last.1
        {
            let last = last.1;
            raised.retain(|(_, seq, _)| *seq != last);
        }

        let mut items = Vec::new();
        for (activity, raised_seq, class) in &raised {
            let inputs = self
                .read_rows(kip::request_with(
                    r#"FIND(?x.id, ?x.schema_ref, ?x.attributes) WHERE {
  ?a ACTIVITY {id: :activity}
  STRUCTURAL (?a, "inputs", ?x)
  ?x CONCEPT {}
} LIMIT :limit"#,
                    serde_json::Map::from_iter([
                        ("activity".to_string(), Json::from(activity.as_str())),
                        ("limit".to_string(), Json::from(MAX_REVIEW_TARGETS)),
                    ]),
                ))
                .await?;
            for input in inputs.iter().filter_map(Json::as_array) {
                let (Some(id), Some(schema_ref)) = (
                    input.first().and_then(Json::as_str),
                    input.get(1).and_then(Json::as_str),
                ) else {
                    continue;
                };
                let attributes = input.get(2).cloned().unwrap_or(Json::Null);
                let item = match class.as_str() {
                    "watch_fire" if schema_ref == profile!("Watch") => AttentionRecallItem {
                        reference: id.to_string(),
                        kind: "watch_fired".into(),
                        summary: summary(&attributes, "Watch fired"),
                        raised_seq: *raised_seq,
                        due_at: None,
                        target_refs: self.watched_targets(id).await?,
                        priority: None,
                    },
                    "commitment_review" if schema_ref == profile!("Commitment") => {
                        AttentionRecallItem {
                            reference: id.to_string(),
                            kind: "commitment_due".into(),
                            summary: summary(&attributes, "Commitment due"),
                            raised_seq: *raised_seq,
                            due_at: attributes["due_at"].as_str().map(str::to_string),
                            target_refs: vec![id.to_string()],
                            priority: attributes["priority"].as_f64(),
                        }
                    }
                    _ => continue,
                };
                items.push(item);
            }
        }
        let delivered = raised.last().map(|(_, seq, _)| *seq).or(after);
        Ok(AttentionRecall {
            items,
            attention_cursor: format!("{CURSOR_PREFIX}{}", delivered.map_or(-1, |seq| seq as i64)),
            complete,
        })
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

/// `attention:-1` is before every raise; `attention:<n>` is after `raised_seq` n.
fn parse_cursor(cursor: &str) -> Result<Option<u64>, BoxError> {
    let invalid = || -> BoxError { "invalid attention cursor".into() };
    let value = cursor.strip_prefix(CURSOR_PREFIX).ok_or_else(invalid)?;
    if value == "-1" {
        return Ok(None);
    }
    value.parse::<u64>().map(Some).map_err(|_| invalid())
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
