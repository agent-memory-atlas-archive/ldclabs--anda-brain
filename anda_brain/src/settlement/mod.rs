//! Bounded maintenance passes. Nexus owns Watch generations, authorization
//! coverage and cognitive record validation; Brain owns scheduling and review.
//! Learning requires a configured observer/trial/evaluation pipeline. The old
//! family-rate verdict is deliberately absent: family membership is not a control.

pub(crate) mod watch;

use anda_core::BoxError;
use anda_kip::{KipError, KipErrorCode, Request, Response};
use serde_json::Value as Json;
use std::future::Future;

use crate::{
    kip,
    types::{
        ArmedWatch, CommitmentSettlement, Dependent, RevisedRoot, SkillSettlement, WatchSettlement,
    },
};

/// Per-command row limit for bulk settlement passes.
pub(crate) const SETTLEMENT_BATCH_LIMIT: usize = 500;

/// The `retention.retention_class` a pinned memory carries.
///
/// KIP 1.x pinning was a `metadata.pinned` flag; 2.0 has no generic metadata
/// bag, and "keep this out of the metabolism ladder" is a storage-lifecycle
/// statement, which is what the retention block is for. Retention also lives on
/// every element kind, so one class keeps pinning working for Concepts and
/// Propositions alike.
pub(crate) const PINNED_RETENTION_CLASS: &str = "pinned";

/// The `retention_class` an unpinned element returns to.
pub(crate) const STANDARD_RETENTION_CLASS: &str = "standard";

/// One command in, one result out: the settlement's whole view of a graph.
///
/// `readonly` picks the gate rather than the caller picking an executor, so a
/// pass cannot reach a write path by choosing the wrong method. Write commands
/// answer `outcome_unknown` on a timeout rather than erroring — the write may
/// already have committed (Spec §80.3), and every settlement command writes
/// absolute values, so a re-run on the next cycle is the honest recovery.
pub(crate) trait RunKip: Send + Sync {
    /// Live Spaces use the same persisted service as the independent scheduler.
    /// Test/legacy ports may retain the bounded scan adapter below.
    fn attention_sweep(&self) -> impl Future<Output = Option<WatchSettlement>> + Send {
        async { None }
    }
    /// The graph being settled. Log context only; nothing branches on it.
    fn space_id(&self) -> &str;

    fn run_kip(
        &self,
        request: Request,
        readonly: bool,
    ) -> impl Future<Output = Result<Response, BoxError>> + Send;

    /// Protected host operation. A KML write cannot attest Watch coverage.
    fn advance_watch(
        &self,
        _id: &str,
        _version: u64,
        _generation: u64,
    ) -> impl Future<Output = Result<Json, KipError>> + Send {
        async {
            Err(KipError::unsupported_capability(
                "Watch runtime is unavailable",
            ))
        }
    }
}

/// Reads one command, or reports why the page is unusable. The passes all
/// degrade the same way — a cycle that could not scan is degraded, not
/// failed — so the two failure shapes (transport error, operation error)
/// collapse to one message here rather than at four call sites.
///
/// Hands back the whole [`Response`] rather than its result value: every row
/// reader borrows, and a page of outcomes is up to `OUTCOME_WINDOW` rows that
/// a settlement over many Skills would otherwise deep-clone once per read.
async fn read(port: &impl RunKip, request: Request) -> Result<Response, String> {
    match port.run_kip(request, true).await {
        Ok(response) if kip::succeeded(&response) => Ok(response),
        Ok(response) => Err(kip::error_message(&response)),
        Err(err) => Err(err.to_string()),
    }
}

/// One Assertion an actor has revised, as the correction scan reads it.
#[derive(Debug, Clone)]
pub(crate) struct SupersededRow {
    pub assertion: String,
    /// The *transaction* coordinate the revision landed at.
    pub space_seq: u64,
    /// Whose claim needed revising. KIP 1.x kept a free-text
    /// `metadata.source`; 2.0 attributes a claim to a semantic actor, and that
    /// actor is the reliability signal — this is not a statement about the
    /// caller's authority.
    pub actor: Option<String>,
    /// The Proposition the claim took a stance on.
    pub proposition: Option<String>,
    /// The Assertions that superseded it.
    pub superseded_by: Vec<String>,
}

/// Durable position inside a transaction coordinate. An empty id means the
/// entire coordinate was read; a nonempty id resumes within that coordinate.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CorrectionCursor {
    pub seq: u64,
    #[serde(default)]
    pub after_id: String,
}

impl From<u64> for CorrectionCursor {
    fn from(seq: u64) -> Self {
        Self {
            seq,
            after_id: String::new(),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct CorrectionScan {
    pub rows: Vec<SupersededRow>,
    pub cursor: CorrectionCursor,
    /// Largest transaction coordinate proved completely read.
    pub watermark: u64,
    /// A bounded page did not prove the backlog exhausted. No rows are lost.
    pub incomplete: bool,
    pub error: Option<String>,
}

pub(crate) async fn scan_corrections(
    port: &impl RunKip,
    after: CorrectionCursor,
) -> CorrectionScan {
    let through = if after.after_id.is_empty() {
        after.seq
    } else {
        after.seq.saturating_sub(1)
    };
    let mut scan = CorrectionScan {
        cursor: after.clone(),
        watermark: through,
        ..Default::default()
    };
    let boundary = if after.after_id.is_empty() {
        "FILTER(?a._system.space_seq > :after)"
    } else {
        "FILTER(?a._system.space_seq > :after || (?a._system.space_seq == :after && ?a.id > :after_id))"
    };
    let request = kip::request_with(
        format!(
            "FIND(?a.id, ?a._system.space_seq, ?a.asserted_by, ?a.proposition, ?a.lifecycle.superseded_by) WHERE {{ ?a ASSERTION {{}} FILTER(?a.lifecycle.status == \"superseded\") {boundary} }} ORDER BY ?a._system.space_seq, ?a.id LIMIT {SETTLEMENT_BATCH_LIMIT}"
        ),
        serde_json::Map::from_iter([
            ("after".into(), Json::from(after.seq)),
            ("after_id".into(), Json::from(after.after_id.clone())),
        ]),
    );
    let response = match read(port, request).await {
        Ok(response) => response,
        Err(error) => {
            scan.incomplete = true;
            scan.error = Some(error);
            return scan;
        }
    };
    let Some(rows) = kip::ok_result(&response).and_then(Json::as_array) else {
        scan.incomplete = true;
        scan.error = Some("correction scan returned no row array".into());
        return scan;
    };
    scan.incomplete = rows.len() >= SETTLEMENT_BATCH_LIMIT
        || response
            .results
            .first()
            .is_some_and(|r| r.next_cursor.is_some());
    let mut parsed = superseded_rows(&Json::Array(rows.clone()));
    if parsed.len() != rows.len() {
        scan.incomplete = true;
        scan.error = Some("correction scan returned an unreadable row; cursor retained".into());
        return scan;
    }
    parsed.retain(|row| {
        row.space_seq > after.seq
            || row.space_seq == after.seq
                && !after.after_id.is_empty()
                && row.assertion > after.after_id
    });
    parsed.sort_by(|a, b| {
        a.space_seq
            .cmp(&b.space_seq)
            .then(a.assertion.cmp(&b.assertion))
    });
    if let Some(last) = parsed.last() {
        scan.cursor = CorrectionCursor {
            seq: last.space_seq,
            after_id: if scan.incomplete {
                last.assertion.clone()
            } else {
                String::new()
            },
        };
        scan.watermark = if scan.incomplete {
            last.space_seq.saturating_sub(1)
        } else {
            last.space_seq
        };
    } else if !scan.incomplete {
        scan.cursor = after.seq.into();
        scan.watermark = after.seq;
    }
    scan.watermark = scan.watermark.max(through);
    scan.rows = parsed;
    scan
}

/// Schedule bounded native Watch advancement. Only Nexus can attest generation,
/// current authorization and deadline coverage. Text conditions stay deferred.
/// Errors remain visible, and no failed Watch is automatically re-armed.
pub(crate) async fn sweep_watches(port: &impl RunKip) -> WatchSettlement {
    if let Some(report) = port.attention_sweep().await {
        return report;
    }
    let mut report = WatchSettlement::default();
    let response = match read(port, watch::watches_request("armed")).await {
        Ok(response) => response,
        Err(error) => {
            report.error = Some(error);
            return report;
        }
    };
    let rows = kip::ok_result(&response)
        .map(watch::read_watch_rows)
        .unwrap_or_default();
    let full = rows.len() >= watch::WATCH_SWEEP_LIMIT;
    let mut runnable = Vec::new();
    for row in rows {
        let Some(_) = row.generation else {
            report.deferred += 1;
            report.error = Some("armed Watch has no WatchState; review its observation gap and re-arm it through the host".into());
            continue;
        };
        // Text (including a text member combined with structured selectors)
        // needs a semantic evaluator. A snapshot or model completion is no proof
        // of having interpreted every authorized change, including empty pages.
        if !watch::is_structured(&row.condition) {
            report.deferred += 1;
            continue;
        }
        runnable.push(row);
    }
    if full {
        match read(port, watch::runnable_watches_request()).await {
            Ok(response) => {
                runnable = kip::ok_result(&response)
                    .map(watch::read_watch_rows)
                    .unwrap_or_default()
            }
            Err(error) => {
                report.error = Some(error);
                return report;
            }
        }
    }
    for row in runnable {
        let Some(generation) = row.generation else {
            continue;
        };
        if !watch::is_structured(&row.condition) {
            continue;
        }
        match port.advance_watch(&row.id, row.version, generation).await {
            Ok(result) => match result["status"].as_str() {
                Some("fired") => report.fired += 1,
                Some("expired" | "disarmed") => report.disarmed += 1,
                _ => report.deferred += 1,
            },
            Err(error) if error.code == KipErrorCode::VersionConflict => {
                report.conflicted += 1;
                report.error = Some(error.to_string());
            }
            Err(error) => {
                report.deferred += 1;
                report.error = Some(error.to_string());
            }
        }
    }
    report
}

/// How many due Commitments one cycle raises; the rest wait for the next.
pub(crate) const COMMITMENT_REVIEW_LIMIT: usize = 50;

/// Due `pending` / `blocked` Commitments that no Watch watches, earliest first.
/// The `due_at` comparison is lexical here and re-checked as a time below.
const DUE_COMMITMENTS: &str = r#"FIND(?c.id, ?c.attributes.due_at) WHERE {
  ?c CONCEPT {type: "Commitment"}
  FILTER((?c.attributes.status == "pending" || ?c.attributes.status == "blocked") && ?c.attributes.due_at <= :now)
  NOT { ?w CONCEPT {type: "Watch"} STRUCTURAL (?w, "watches", ?c) }
} ORDER BY ?c.attributes.due_at ASC LIMIT :limit"#;

/// One `commitment_review` Activity raising one Commitment (Profile §5.7, §17).
const RAISE_COMMITMENT: &str = r#"CREATE ACTIVITY ?review {
  CLIENT KEY :key
  SET FIELDS { activity_class: "commitment_review", status: "completed", started_at: :now, ended_at: :now }
  SET STRUCTURAL { ("inputs", :commitment) }
}"#;

/// Raises every due Commitment without a Watch as `commitment_due` attention.
///
/// Native rather than left to the maintenance model: the attention stream is
/// what a business agent acts on, so a Commitment has to reach it whether or
/// not a model thought to look, and exactly once per `due_at`. The CLIENT KEY
/// `commitment_review:<id>:<due_at>` is what makes a replay `no_effect` and a
/// rescheduled Commitment rise again (Profile §5.7, §17; Memory Interface §4).
/// A due time passing changes nothing else: the Commitment keeps its status.
pub(crate) async fn raise_due_commitments(port: &impl RunKip, now_ms: u64) -> CommitmentSettlement {
    let now = kip::timestamp(now_ms);
    let mut report = CommitmentSettlement::default();
    let response = match read(
        port,
        kip::request_with(
            DUE_COMMITMENTS,
            serde_json::Map::from_iter([
                ("now".to_string(), Json::from(now.as_str())),
                ("limit".to_string(), Json::from(COMMITMENT_REVIEW_LIMIT)),
            ]),
        ),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            report.error = Some(error);
            return report;
        }
    };
    let rows = kip::ok_result(&response)
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    for row in rows {
        let (Some(id), Some(due_at)) = (
            row.get(0).and_then(Json::as_str),
            row.get(1).and_then(Json::as_str),
        ) else {
            continue;
        };
        if kip::time_ms(due_at).is_none_or(|due| due > now_ms) {
            continue;
        }
        report.due += 1;
        let request = kip::request_with(
            RAISE_COMMITMENT,
            serde_json::Map::from_iter([
                (
                    "key".to_string(),
                    Json::from(format!("commitment_review:{id}:{due_at}")),
                ),
                ("commitment".to_string(), Json::from(id)),
                ("now".to_string(), Json::from(now.as_str())),
            ]),
        );
        match port.run_kip(request, false).await {
            Ok(response) if kip::succeeded(&response) => {
                if kip::changed(&response, "create") > 0 {
                    report.raised += 1;
                }
            }
            Ok(response) => report.error = Some(kip::error_message(&response)),
            Err(error) => report.error = Some(error.to_string()),
        }
    }
    report
}

/// The Watches in one status, as the maintenance prompt receives them.
///
/// Best-effort: an error here costs the cycle one input, not the cycle. A
/// Space that has never declared a Watch resolves the type fine (the Profile
/// declares it), so a failure is a real one and is logged as such.
pub(crate) async fn watches_in_status(port: &impl RunKip, status: &str) -> Vec<ArmedWatch> {
    match read(port, watch::watches_request(status)).await {
        Ok(response) => kip::ok_result(&response)
            .map(watch::read_watch_rows)
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.watch)
            .collect(),
        Err(error) => {
            log::warn!(
                target: "brain",
                space_id = port.space_id(),
                status;
                "reading Watches for the maintenance assessment failed: {error}"
            );
            Vec::new()
        }
    }
}

/// How many revised roots one cycle walks.
pub(crate) const REVISED_ROOTS_LIMIT: usize = 20;

/// How far a derivation walk follows Activity lineage from a revised root.
pub(crate) const DEPENDENTS_DEPTH: u64 = 2;

/// How many dependents one walk lists.
pub(crate) const DEPENDENTS_LIMIT: usize = 20;

/// The cognition derived from each newly superseded Assertion.
///
/// Spec §57.5 asks a Brain to review the dependents of a revised root, and
/// §63.5 makes them discoverable in one read — so the runtime reads them and
/// hands the cycle a list, where before it handed a count. Reachability is
/// topology, not judgment: nothing here is flagged stale; the cycle decides.
pub(crate) async fn revised_roots(port: &impl RunKip, rows: &[SupersededRow]) -> Vec<RevisedRoot> {
    let mut roots = Vec::with_capacity(rows.len().min(REVISED_ROOTS_LIMIT));
    for row in rows.iter().take(REVISED_ROOTS_LIMIT) {
        let request = kip::request_with(
            format!("LIST DEPENDENTS :root DEPTH {DEPENDENTS_DEPTH} LIMIT {DEPENDENTS_LIMIT}"),
            kip::param("root", row.assertion.as_str()),
        );
        let (dependents, truncated) = match read(port, request).await {
            Ok(response) => (dependents_of(&response), dependents_truncated(&response)),
            Err(error) => {
                log::warn!(
                    target: "brain",
                    space_id = port.space_id(),
                    assertion = row.assertion;
                    "walking the dependents of a revised root failed: {error}"
                );
                (Vec::new(), true)
            }
        };
        roots.push(RevisedRoot {
            assertion: row.assertion.clone(),
            proposition: row.proposition.clone(),
            actor: row.actor.clone(),
            superseded_by: row.superseded_by.clone(),
            space_seq: row.space_seq,
            dependents,
            truncated,
        });
    }
    roots
}

/// The rows of a `LIST DEPENDENTS` answer — `{id, kind, distance, via}`.
fn dependents_of(response: &Response) -> Vec<Dependent> {
    kip::ok_result(response)
        .and_then(Json::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(Dependent {
                        id: row.get("id")?.as_str()?.to_string(),
                        kind: row
                            .get("kind")
                            .and_then(Json::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        distance: row.get("distance").and_then(Json::as_u64).unwrap_or(1),
                        via: row
                            .get("via")
                            .and_then(|via| via.get("activity"))
                            .and_then(Json::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the walk said it was cut short: the §63.5 `truncated` caveat, or
/// a full page.
fn dependents_truncated(response: &Response) -> bool {
    let Some(result) = response.results.first() else {
        return false;
    };
    result.next_cursor.is_some()
        || result.warnings.iter().any(|warning| {
            matches!(warning, anda_kip::Warning::Coded { code, .. } if code == "truncated")
        })
}

/// No learning scheduler is configured by this deployment. Preserve counters
/// for response compatibility, but disclose that no evaluation was performed.
pub(crate) fn skill_settlement() -> SkillSettlement {
    SkillSettlement {
        unsupported_reason: Some("memory_learning requires configured independent observers, frozen trials and replayable evaluations".into()),
        ..Default::default()
    }
}

/// Reads the rows of the correction-discovery scan —
/// `FIND(?a.id, ?a._system.space_seq, ?a.asserted_by, ?a.proposition,
/// ?a.lifecycle.superseded_by)`.
fn superseded_rows(result: &Json) -> Vec<SupersededRow> {
    let element_id = |value: &Json| -> Option<String> {
        match value {
            Json::String(id) => Some(id.clone()),
            Json::Object(map) => map.get("id").and_then(Json::as_str).map(str::to_string),
            _ => None,
        }
    };
    result
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let columns = row.as_array()?;
                    let id = columns.first().and_then(Json::as_str)?;
                    Some(SupersededRow {
                        assertion: id.to_string(),
                        space_seq: columns.get(1).and_then(Json::as_u64)?,
                        actor: columns.get(2).and_then(element_id),
                        proposition: columns.get(3).and_then(element_id),
                        superseded_by: columns
                            .get(4)
                            .and_then(Json::as_array)
                            .map(|ids| ids.iter().filter_map(element_id).collect())
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anda_kip::{KipError, KipErrorCode};
    use parking_lot::Mutex;
    use serde_json::json;
    use std::collections::VecDeque;

    /// The settlement's other adapter: a scripted graph.
    ///
    /// Answers are keyed by a substring of the command text — enough to tell
    /// the passes' reads apart — and each key holds a queue, so a paging pass
    /// can be walked batch by batch. An unmatched command answers with an
    /// empty result rather than failing: a pass that reads something the test
    /// did not script should behave as it does against a graph that has
    /// nothing to say, not blow up somewhere unrelated.
    #[derive(Default)]
    struct FakeKip {
        replies: Mutex<Vec<(&'static str, VecDeque<Response>)>>,
        seen: Mutex<Vec<(String, bool)>>,
    }

    impl FakeKip {
        fn new(replies: impl IntoIterator<Item = (&'static str, Vec<Response>)>) -> Self {
            Self {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|(key, queue)| (key, VecDeque::from(queue)))
                        .collect(),
                ),
                seen: Mutex::new(Vec::new()),
            }
        }

        /// Every command the passes issued, with the gate they asked for.
        fn seen(&self) -> Vec<(String, bool)> {
            self.seen.lock().clone()
        }

        fn wrote(&self) -> Vec<String> {
            self.seen()
                .into_iter()
                .filter(|(_, readonly)| !readonly)
                .map(|(command, _)| command)
                .collect()
        }
    }

    impl RunKip for FakeKip {
        fn space_id(&self) -> &str {
            "fake_space"
        }

        async fn run_kip(&self, request: Request, readonly: bool) -> Result<Response, BoxError> {
            let command = request
                .operations
                .first()
                .and_then(|op| op.command.clone())
                .unwrap_or_default();
            self.seen.lock().push((command.clone(), readonly));
            let mut replies = self.replies.lock();
            for (key, queue) in replies.iter_mut() {
                if command.contains(*key)
                    && let Some(response) = queue.pop_front()
                {
                    return Ok(response);
                }
            }
            Ok(Response::ok(json!([])))
        }
    }

    fn failed(message: &str) -> Response {
        Response::failed(KipError::new(KipErrorCode::InternalError, message))
    }

    /// The correction cursor advances only over coordinates it read whole.
    #[tokio::test]
    async fn the_correction_cursor_never_steps_over_a_half_read_coordinate() {
        // A short page reached the end of the backlog: every coordinate in it
        // was read whole, so the cursor takes the last one.
        let scan =
            scan_corrections(&superseded(&[(7, "actor_a"), (9, "actor_b")]), 3u64.into()).await;
        assert_eq!(scan.watermark, 9);
        assert_eq!(scan.rows.len(), 2);
        assert_eq!(scan.rows[0].actor.as_deref(), Some("actor_a"));
        assert!(!scan.incomplete);

        // Nothing to read leaves the cursor alone.
        assert_eq!(
            scan_corrections(&superseded(&[]), 3u64.into())
                .await
                .watermark,
            3
        );

        // A full page stops one coordinate short: `space_seq` is the
        // transaction coordinate, so the trailing 9s may have more behind them
        // and the next scan has to see them again.
        let mut page: Vec<(u64, &str)> = vec![(7, "a"), (8, "a")];
        page.extend((0..SETTLEMENT_BATCH_LIMIT - 2).map(|_| (9u64, "a")));
        let full = scan_corrections(&superseded(&page), 3u64.into()).await;
        assert_eq!(full.watermark, 8);
        assert!(full.incomplete);
        assert!(!full.cursor.after_id.is_empty());

        // Never backwards, whatever the page held: a cursor already past the
        // coordinate this page earned stays where it is, so corrections
        // already recorded are not scanned again forever.
        let ahead = scan_corrections(&superseded(&page), 8u64.into()).await;
        assert_eq!(ahead.watermark, 8);
        let further = scan_corrections(&superseded(&page), 42u64.into()).await;
        assert_eq!(further.watermark, 42);

        // A full page within one coordinate resumes after its last id;
        // it never declares the unread tail consumed.
        let one: Vec<(u64, &str)> = (0..SETTLEMENT_BATCH_LIMIT).map(|_| (9u64, "a")).collect();
        let stuck = scan_corrections(&superseded(&one), 3u64.into()).await;
        assert_eq!(stuck.watermark, 8);
        assert_eq!(stuck.cursor.seq, 9);
        assert!(!stuck.cursor.after_id.is_empty());
        assert!(stuck.incomplete);
    }

    #[tokio::test]
    async fn a_failed_correction_scan_leaves_the_cursor_where_it_was() {
        let port = FakeKip::new([("superseded", vec![failed("full-scan cap")])]);
        let scan = scan_corrections(&port, 42u64.into()).await;

        assert_eq!(scan.watermark, 42);
        assert!(scan.rows.is_empty());
        assert!(scan.error.unwrap().contains("full-scan cap"));
        // Reading corrections never writes.
        assert!(port.wrote().is_empty());
    }

    #[tokio::test]
    async fn a_transaction_larger_than_one_page_is_fully_discovered() {
        let rows: Vec<Json> = (0..SETTLEMENT_BATCH_LIMIT)
            .map(|i| {
                serde_json::json!([
                    format!("A-{}", 1000+i), 9, {"id":"C-1"}, {"id":"P-1"}, [{"id":"A-9999"}]
                ])
            })
            .collect();
        let port = FakeKip::new([(
            "superseded",
            vec![
                Response::ok(Json::Array(rows)),
                Response::ok(
                    serde_json::json!([["A-2000",9,{"id":"C-1"},{"id":"P-1"},[{"id":"A-9999"}]]]),
                ),
            ],
        )]);
        let first = scan_corrections(&port, 0u64.into()).await;
        assert_eq!(first.rows.len(), SETTLEMENT_BATCH_LIMIT);
        assert!(first.incomplete);
        assert_eq!(first.watermark, 8);
        let second = scan_corrections(&port, first.cursor).await;
        assert_eq!(second.rows.len(), 1);
        assert_eq!(second.rows[0].assertion, "A-2000");
        assert!(!second.incomplete);
        assert_eq!(second.watermark, 9);
        assert!(second.cursor.after_id.is_empty());
    }

    /// Builds a port whose correction scan answers with these
    /// `(space_seq, actor)` rows. Whether the page is full is derived from its
    /// length, exactly as the pass derives it, so a test cannot claim a full
    /// page it did not supply.
    fn superseded(rows: &[(u64, &str)]) -> FakeKip {
        let result: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(n, (seq, actor))| json!([format!("A-{n}"), seq, actor, "P-1", ["A-99"]]))
            .collect();
        FakeKip::new([("superseded", vec![Response::ok(json!(result))])])
    }

    #[tokio::test]
    async fn revised_roots_carry_what_each_superseded_claim_fed() {
        let rows = vec![
            SupersededRow {
                assertion: "A-3".into(),
                space_seq: 40,
                actor: Some("C-7".into()),
                proposition: Some("P-11".into()),
                superseded_by: vec!["A-9".into()],
            },
            SupersededRow {
                assertion: "A-4".into(),
                space_seq: 41,
                actor: None,
                proposition: None,
                superseded_by: vec![],
            },
        ];
        let port = FakeKip::new([(
            "LIST DEPENDENTS",
            vec![
                Response::ok(json!([
                    {"id": "C-30", "kind": "concept", "distance": 1, "via": {"activity": "ACT-5"}},
                    {"id": "C-31", "kind": "concept", "distance": 2, "via": {"activity": "ACT-6"}}
                ])),
                failed("engine busy"),
            ],
        )]);
        let roots = revised_roots(&port, &rows).await;

        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].assertion, "A-3");
        assert_eq!(roots[0].superseded_by, vec!["A-9".to_string()]);
        assert_eq!(roots[0].dependents.len(), 2);
        assert_eq!(roots[0].dependents[0].id, "C-30");
        assert_eq!(roots[0].dependents[0].via.as_deref(), Some("ACT-5"));
        assert!(!roots[0].truncated);
        // A walk that failed says the list is not to be trusted, rather than
        // reporting a root with no derivations.
        assert!(roots[1].dependents.is_empty());
        assert!(roots[1].truncated);
        // Reads only: a derivation walk changes nothing.
        assert!(port.wrote().is_empty());
    }

    #[test]
    fn learning_is_explicitly_unavailable_without_a_pipeline() {
        let report = skill_settlement();
        assert_eq!(report.graded, 0);
        assert_eq!(report.transitions, 0);
        assert!(report.unsupported_reason.is_some());
    }
}
