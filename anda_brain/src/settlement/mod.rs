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
    types::{ArmedWatch, Dependent, MemoryPolicy, RevisedRoot, SkillSettlement, WatchSettlement},
};

/// Per-command row limit for bulk settlement passes.
pub(crate) const SETTLEMENT_BATCH_LIMIT: usize = 500;

/// Upper bound of decay batches per settlement (500 × 20 = 10k links).
const SETTLEMENT_MAX_BATCHES: usize = 20;

/// `MnemonicState.memory_strength` assumed for a Concept that has never been
/// metabolized. The Facet's members are all optional, so the metabolism has to
/// supply a baseline before it can decay one.
const DEFAULT_MEMORY_STRENGTH: f64 = 0.5;

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

/// What the bulk disuse metabolism did this cycle.
#[derive(Debug, Default)]
pub(crate) struct DecayPass {
    /// Concepts whose `MnemonicState.memory_strength` was decayed.
    pub decayed: u64,
    /// Set when the pass stopped early — decay did not complete this cycle.
    pub error: Option<String>,
}

/// Bulk disuse metabolism: decays `MnemonicState.memory_strength` — how
/// available a memory should be — and never Assertion confidence. A fact
/// nobody asked about lately is no less credible, and KIP 2.0 forbids letting
/// time erode a stance (Profile: "Do not decay epistemic confidence merely
/// because a fact has not been recalled recently"). A failing pass degrades —
/// the surrounding passes still run — but it must page an operator rather than
/// vanish into a debug log.
///
/// The cadence is `min_interval_ms`, enforced inside the sweep's own
/// `last_metabolized_at` filter, not the cycle scope. Gating on the `full`
/// scope as well used to look like caution and was a hole: full cycles are
/// scheduled every 168 formations, so a Space forming slowly went months
/// without metabolizing while `BrainMaintenance.md` §A.1 told the model the
/// sweep had already run and not to do it by hand. With the interval doing the
/// throttling, a scope that has nothing due costs one query that matches no
/// rows and breaks on the first batch.
pub(crate) async fn metabolize(
    port: &impl RunKip,
    policy: &MemoryPolicy,
    now_ms: u64,
    min_interval_ms: u64,
) -> DecayPass {
    let mut pass = DecayPass::default();
    // Every input is loop-invariant, so the command is too: the batch cursor
    // is the graph's own `last_metabolized_at` filter, not anything this
    // builder carries.
    let request = decay_request(policy, now_ms, min_interval_ms);
    for _ in 0..SETTLEMENT_MAX_BATCHES {
        let response = match port.run_kip(request.clone(), false).await {
            Ok(response) if kip::succeeded(&response) => response,
            Ok(response) => {
                pass.error = Some(kip::error_message(&response));
                break;
            }
            Err(err) => {
                pass.error = Some(err.to_string());
                break;
            }
        };
        let updated = kip::changed(&response, "update");
        pass.decayed += updated;
        if updated < SETTLEMENT_BATCH_LIMIT as u64 {
            break;
        }
    }
    if let Some(error) = &pass.error {
        log::error!(
            target: "brain",
            space_id = port.space_id();
            "memory-strength metabolism failed — disuse decay is NOT running \
             (graph past the full-scan engine cap?): {error}"
        );
    }
    pass
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

/// One page of correction discovery: what was superseded, and how far the
/// cursor may advance for having read it.
#[derive(Debug, Default)]
pub(crate) struct CorrectionScan {
    pub rows: Vec<SupersededRow>,
    /// The cursor this page earned — never past a coordinate it only half
    /// read. Equal to the caller's `after` when nothing was consumed.
    pub watermark: u64,
    /// Set when one transaction superseded more Assertions than a page holds,
    /// so the remainder at that coordinate is lost.
    ///
    /// The operator signal is the error log the scan writes; this field is
    /// how a test asserts the truncation arm fired without reading logs. The
    /// settlement report has no field for it, and adding one would change a
    /// persisted shape to carry something only a test reads.
    pub truncated_at: Option<u64>,
    /// Set when the scan failed — new corrections were not recorded this
    /// cycle.
    pub error: Option<String>,
}

/// Correction discovery: the Assertions an actor has revised since `after`.
///
/// In KIP 1.x this was a `metadata.superseded` flag the settlement could clear
/// with a second write; an Assertion is immutable, so the cursor is the Space
/// sequence coordinate instead — processed revisions fall behind the
/// watermark, and a backlog larger than one batch drains across cycles rather
/// than starving new corrections behind the first LIMIT-full.
///
/// Recording what comes back is the caller's: the usage ledger dedupes
/// corrections and the `source_reliability` aggregate is Space extension
/// state, neither of which the graph owns.
pub(crate) async fn scan_corrections(port: &impl RunKip, after: u64) -> CorrectionScan {
    let mut scan = CorrectionScan {
        watermark: after,
        ..Default::default()
    };

    let request = kip::request_with(
        format!(
            "FIND(?a.id, ?a._system.space_seq, ?a.asserted_by, ?a.proposition, ?a.lifecycle.superseded_by) WHERE {{\n  ?a ASSERTION {{}}\n  FILTER(?a.lifecycle.status == \"superseded\")\n  FILTER(?a._system.space_seq > :after)\n}}\nORDER BY ?a._system.space_seq\nLIMIT {SETTLEMENT_BATCH_LIMIT}"
        ),
        kip::param("after", after),
    );
    let response = match read(port, request).await {
        Ok(response) => response,
        Err(error) => {
            log::error!(
                target: "brain",
                space_id = port.space_id();
                "correction discovery scan failed — new corrections are NOT being \
                 recorded (graph past the full-scan engine cap?): {error}"
            );
            scan.error = Some(error);
            return scan;
        }
    };

    let Some(rows) = kip::ok_result(&response) else {
        return scan;
    };
    let page_full = rows
        .as_array()
        .is_some_and(|rows| rows.len() >= SETTLEMENT_BATCH_LIMIT);
    scan.rows = superseded_rows(rows);
    let seqs: Vec<u64> = scan.rows.iter().map(|row| row.space_seq).collect();
    scan.watermark = match correction_watermark(&seqs, page_full, after) {
        CorrectionWatermark::Consumed(seq) => seq,
        CorrectionWatermark::Truncated(seq) => {
            log::error!(
                target: "brain",
                space_id = port.space_id(),
                space_seq = seq;
                "one transaction superseded more Assertions than a settlement page \
                 holds ({SETTLEMENT_BATCH_LIMIT}); the remainder at this coordinate \
                 will not be recorded as corrections"
            );
            scan.truncated_at = Some(seq);
            seq
        }
    };
    scan
}

/// Schedule bounded native Watch advancement. Only Nexus can attest generation,
/// current authorization and deadline coverage. Text conditions stay deferred.
/// Errors remain visible, and no failed Watch is automatically re-armed.
pub(crate) async fn sweep_watches(port: &impl RunKip) -> WatchSettlement {
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
            report.error = Some("legacy Watch has no WatchState; explicitly re-arm after reviewing its observation gap".into());
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

/// Weekly disuse changes accessibility only, never Assertion confidence.
fn decay_request(policy: &MemoryPolicy, now_ms: u64, min_interval_ms: u64) -> Request {
    let parameters = serde_json::Map::from_iter([
        ("baseline".to_string(), Json::from(DEFAULT_MEMORY_STRENGTH)),
        (
            "factor".to_string(),
            Json::from(policy.memory_strength_decay_factor),
        ),
        ("floor".to_string(), Json::from(policy.decay_floor)),
        ("now".to_string(), Json::from(kip::timestamp(now_ms))),
        (
            "metabolized_before".to_string(),
            Json::from(kip::timestamp(now_ms.saturating_sub(min_interval_ms))),
        ),
        ("pinned".to_string(), Json::from(PINNED_RETENTION_CLASS)),
        ("limit".to_string(), Json::from(SETTLEMENT_BATCH_LIMIT)),
    ]);
    kip::request_with(
        r#"UPDATE ?c
SET FACET "MnemonicState" {
  memory_strength: CLAMP(MUL(COALESCE(?c.facets["MnemonicState"].memory_strength, :baseline), :factor), :floor, 1.0),
  last_metabolized_at: :now
}
WHERE {
  ?c CONCEPT {}
  NOT { ?c CONCEPT {type: "SleepTask"} }
  NOT { ?c CONCEPT {type: "Watch"} }
  FILTER(IS_NULL(?c.retention.retention_class) || ?c.retention.retention_class != :pinned)
  FILTER(IS_NULL(?c.facets["MnemonicState"].last_metabolized_at) || ?c.facets["MnemonicState"].last_metabolized_at < :metabolized_before)
  FILTER(IS_NULL(?c.facets["MnemonicState"].memory_strength) || ?c.facets["MnemonicState"].memory_strength > :floor)
}
LIMIT :limit"#,
        parameters,
    )
}

/// How far a correction page may advance the cursor, and whether anything was
/// lost getting there.
enum CorrectionWatermark {
    /// Every coordinate up to this one was read whole.
    Consumed(u64),
    /// One coordinate held more rows than a page, so advancing past it drops
    /// the remainder. Reported so an operator hears about it.
    Truncated(u64),
}

/// Chooses the cursor a correction page has actually earned.
///
/// `_system.space_seq` is the *transaction* coordinate, so one commit stamps
/// every Assertion it revised with the same number, and the scan's `>` filter
/// cannot page inside one coordinate. A full page therefore hands its trailing
/// coordinate back and stops one short of it — re-reading costs nothing,
/// because `record_correction` dedupes, while advancing past a half-read
/// coordinate drops the rest of it for good.
///
/// The one case with no good answer is a full page that is *entirely* one
/// coordinate: standing still re-reads it forever and never reaches the
/// corrections behind it, so the cursor advances and says so.
fn correction_watermark(seqs: &[u64], page_full: bool, after: u64) -> CorrectionWatermark {
    let Some(last) = seqs.last().copied() else {
        return CorrectionWatermark::Consumed(after);
    };
    if !page_full {
        // A short page means the scan reached the end of the backlog, so every
        // coordinate in it was read whole.
        return CorrectionWatermark::Consumed(last.max(after));
    }
    match seqs.iter().copied().filter(|seq| *seq < last).max() {
        Some(seq) => CorrectionWatermark::Consumed(seq.max(after)),
        None => CorrectionWatermark::Truncated(last),
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

    /// A committed mutation that moved `count` rows under `op`.
    fn changed(op: &str, count: usize) -> Response {
        Response::ok(json!({
            "changes": (0..count).map(|_| json!({"op": op})).collect::<Vec<_>>()
        }))
    }

    fn failed(message: &str) -> Response {
        Response::failed(KipError::new(KipErrorCode::InternalError, message))
    }

    #[tokio::test]
    async fn metabolism_pages_until_a_batch_comes_back_short() {
        let port = FakeKip::new([(
            "MnemonicState",
            vec![
                changed("update", SETTLEMENT_BATCH_LIMIT),
                changed("update", SETTLEMENT_BATCH_LIMIT),
                changed("update", 7),
            ],
        )]);
        let pass = metabolize(&port, &MemoryPolicy::default(), 1_000, 0).await;

        assert_eq!(pass.decayed, SETTLEMENT_BATCH_LIMIT as u64 * 2 + 7);
        assert!(pass.error.is_none());
        // A short batch ends the pass: no fourth command was issued.
        assert_eq!(port.seen().len(), 3);
        // Decay is a write, whatever it decays.
        assert_eq!(port.wrote().len(), 3);
    }

    #[tokio::test]
    async fn a_failing_metabolism_reports_what_it_managed_and_stops() {
        let port = FakeKip::new([(
            "MnemonicState",
            vec![
                changed("update", SETTLEMENT_BATCH_LIMIT),
                failed("scan cap"),
            ],
        )]);
        let pass = metabolize(&port, &MemoryPolicy::default(), 1_000, 0).await;

        // The batch that did land is still reported — a degraded cycle, not a
        // cycle that did nothing.
        assert_eq!(pass.decayed, SETTLEMENT_BATCH_LIMIT as u64);
        assert!(pass.error.unwrap().contains("scan cap"));
        assert_eq!(port.seen().len(), 2);
    }

    #[tokio::test]
    async fn the_metabolism_never_runs_past_its_batch_ceiling() {
        // A graph that always answers "a full page" would page forever
        // without the ceiling.
        let port = FakeKip::new([(
            "MnemonicState",
            (0..SETTLEMENT_MAX_BATCHES + 5)
                .map(|_| changed("update", SETTLEMENT_BATCH_LIMIT))
                .collect(),
        )]);
        let pass = metabolize(&port, &MemoryPolicy::default(), 1_000, 0).await;

        assert_eq!(port.seen().len(), SETTLEMENT_MAX_BATCHES);
        assert_eq!(
            pass.decayed,
            (SETTLEMENT_MAX_BATCHES * SETTLEMENT_BATCH_LIMIT) as u64
        );
    }

    /// The correction cursor advances only over coordinates it read whole.
    #[tokio::test]
    async fn the_correction_cursor_never_steps_over_a_half_read_coordinate() {
        // A short page reached the end of the backlog: every coordinate in it
        // was read whole, so the cursor takes the last one.
        let scan = scan_corrections(&superseded(&[(7, "actor_a"), (9, "actor_b")]), 3).await;
        assert_eq!(scan.watermark, 9);
        assert_eq!(scan.rows.len(), 2);
        assert_eq!(scan.rows[0].actor.as_deref(), Some("actor_a"));
        assert!(scan.truncated_at.is_none());

        // Nothing to read leaves the cursor alone.
        assert_eq!(scan_corrections(&superseded(&[]), 3).await.watermark, 3);

        // A full page stops one coordinate short: `space_seq` is the
        // transaction coordinate, so the trailing 9s may have more behind them
        // and the next scan has to see them again.
        let mut page: Vec<(u64, &str)> = vec![(7, "a"), (8, "a")];
        page.extend((0..SETTLEMENT_BATCH_LIMIT - 2).map(|_| (9u64, "a")));
        let full = scan_corrections(&superseded(&page), 3).await;
        assert_eq!(full.watermark, 8);
        assert!(full.truncated_at.is_none());

        // Never backwards, whatever the page held: a cursor already past the
        // coordinate this page earned stays where it is, so corrections
        // already recorded are not scanned again forever.
        let ahead = scan_corrections(&superseded(&page), 8).await;
        assert_eq!(ahead.watermark, 8);
        let further = scan_corrections(&superseded(&page), 42).await;
        assert_eq!(further.watermark, 42);

        // A full page that is entirely one coordinate has no good answer:
        // standing still would re-read it forever, so it advances and says so.
        let one: Vec<(u64, &str)> = (0..SETTLEMENT_BATCH_LIMIT).map(|_| (9u64, "a")).collect();
        let stuck = scan_corrections(&superseded(&one), 3).await;
        assert_eq!(stuck.watermark, 9);
        assert_eq!(stuck.truncated_at, Some(9));
    }

    #[tokio::test]
    async fn a_failed_correction_scan_leaves_the_cursor_where_it_was() {
        let port = FakeKip::new([("superseded", vec![failed("full-scan cap")])]);
        let scan = scan_corrections(&port, 42).await;

        assert_eq!(scan.watermark, 42);
        assert!(scan.rows.is_empty());
        assert!(scan.error.unwrap().contains("full-scan cap"));
        // Reading corrections never writes.
        assert!(port.wrote().is_empty());
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
